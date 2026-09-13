//! The persistent runtime's durable identity record (issue #352).
//!
//! One JSON file per namespace under `<state>/runtime/<name>.json`, written
//! by `zirv session serve` and read by every client that wants to know
//! whether a runtime is there and whose it is. Issue #352 asks for five
//! things on it by name -- owner, version, endpoint, creation time and
//! last-client time -- plus two rules about how it may be trusted:
//!
//! - **Stale records are disambiguated by process START IDENTITY, not by pid
//!   alone.** A pid is recycled; a (pid, start time) pair is not, within any
//!   horizon that matters here. [`classify`] is pure over an injected
//!   [`ProcessIdentity`] probe precisely so both directions -- "the pid is
//!   gone" and "the pid is alive but belongs to somebody else now" -- are
//!   provable on every platform, including the one whose real probe cannot
//!   be exercised from the other.
//! - **A restarted service never reuses another process's session identity.**
//!   Every record carries an `instance` minted fresh at each service start
//!   (never derived from the pid, the name or the endpoint), and
//!   `session::host`'s restore path mints a NEW session id for every restored
//!   session, recording the old one only as the conversation to resume. So
//!   the identity a crashed service published can never be re-published by
//!   its successor, even for the same agent in the same directory.
//!
//! Nothing here reads or writes a pty, and nothing here decides policy: the
//! record is data, and `session::service` is what acts on it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::state::{self, StateDir};

/// The namespace a client means when it names none. One runtime per operator
/// per machine is the shape issue #352 describes; the field exists because
/// the issue says "namespace(s)", and a name is cheaper to add now than to
/// retrofit onto a path layout that assumed one.
pub const DEFAULT_NAMESPACE: &str = "default";

/// How far two independent readings of the same process's start time may
/// differ before they are taken to be two different processes. Deliberately
/// the same 300s `sessions::START_TIME_TOLERANCE_SECS` uses, and for the same
/// reason: a recycled pid shows up hours or days later, while an NTP step can
/// move either reading by seconds without any process having changed.
pub const START_TOLERANCE_SECS: u64 = 300;

/// Who published a namespace record, in enough detail to tell the publisher
/// apart from whatever else may hold its pid later.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    /// Epoch seconds the process holding `pid` started, where the platform
    /// can say. `None` is "cannot tell" and must never be read as a
    /// mismatch -- see [`classify`].
    #[serde(default)]
    pub start: Option<u64>,
    /// The operator account, for a human-readable mismatch message. Never
    /// used as an authorization decision: the endpoint's own owner-only
    /// permissions are that (see `api::transport`).
    #[serde(default)]
    pub user: Option<String>,
}

/// One runtime namespace, exactly as it sits on disk.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Namespace {
    pub name: String,
    pub owner: Owner,
    /// The publishing binary's crate version, so a client can say "the
    /// runtime is older than you" rather than failing obscurely.
    pub version: String,
    pub protocol: u32,
    /// The endpoint path, in `state::display_path` form -- for the operator
    /// to read, never for a client to dial: a client derives the endpoint
    /// from its own resolved state directory exactly like the server does,
    /// so no file on disk can point one at an arbitrary socket.
    pub endpoint: String,
    pub created_at: u64,
    /// When a client last attached, detached or called. Advisory only:
    /// staleness is decided by start identity first.
    pub last_client_at: u64,
    /// Minted fresh at every service start. The anchor for "a crashed or
    /// restarted service cannot reuse another process's session identity".
    pub instance: String,
    /// Whether this runtime was started with tier-3 terminal history on. Kept
    /// on the record so `zirv session list` can say so without loading the
    /// operator's config, and so a client can warn about a runtime somebody
    /// else started with it enabled.
    #[serde(default)]
    pub history: bool,
}

/// What a liveness probe could learn about one pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessIdentity {
    pub alive: bool,
    pub start: Option<u64>,
}

/// What a namespace record is worth right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The publishing process is alive AND its start identity still matches.
    Live,
    /// The publishing process is gone. Safe to replace.
    Gone,
    /// The pid is alive but belongs to a different process than the one that
    /// wrote this record -- a recycled pid. Safe to replace, and worth saying
    /// out loud, because this is exactly the case a pid-only check gets
    /// wrong.
    Recycled,
    /// The pid is alive but this platform (or this record) cannot supply a
    /// start identity to confirm it with, and the heartbeat has gone quiet.
    /// Deliberately its own answer rather than folded into `Live` or `Gone`:
    /// claiming either would be inventing evidence.
    Unverified,
}

/// Whether this record may be replaced by a new service without stepping on a
/// live one.
impl Liveness {
    pub fn is_replaceable(self) -> bool {
        matches!(self, Liveness::Gone | Liveness::Recycled)
    }

    pub fn label(self) -> &'static str {
        match self {
            Liveness::Live => "live",
            Liveness::Gone => "gone",
            Liveness::Recycled => "stale (pid recycled)",
            Liveness::Unverified => "unverified (no start identity, heartbeat quiet)",
        }
    }
}

/// The disambiguation itself. Pure over `probe` and `now`, so both the
/// recycled-pid case and the missing-identity case are testable without
/// arranging a real recycled pid on either platform.
///
/// Order matters and is the whole point: the pid is consulted first only to
/// rule a record OUT (a dead pid is unambiguously gone), and a live pid is
/// never enough on its own to rule one IN.
pub fn classify(
    record: &Namespace,
    probe: &dyn Fn(u32) -> ProcessIdentity,
    now: u64,
    stale_after_secs: u64,
) -> Liveness {
    let identity = probe(record.owner.pid);
    if !identity.alive {
        return Liveness::Gone;
    }
    match (record.owner.start, identity.start) {
        (Some(recorded), Some(current)) => {
            if recorded.abs_diff(current) > START_TOLERANCE_SECS {
                Liveness::Recycled
            } else {
                Liveness::Live
            }
        }
        // No start identity on one side or the other. The pid alone is not
        // evidence, so a quiet heartbeat is reported as unverified rather
        // than as either a live runtime or a dead one.
        _ => {
            if now.saturating_sub(record.last_client_at) > stale_after_secs {
                Liveness::Unverified
            } else {
                Liveness::Live
            }
        }
    }
}

/// The real probe. Unix reuses `sessions`'s own liveness and `ps`-derived
/// start time rather than adding a second reader; Windows asks the kernel for
/// the process creation time directly, which is the one place the two
/// platforms genuinely differ (`sessions::process_start_secs` is `None` off
/// unix by construction).
pub fn process_identity(pid: u32) -> ProcessIdentity {
    let alive = super::super::sessions::is_alive(pid);
    if !alive {
        return ProcessIdentity {
            alive: false,
            start: None,
        };
    }
    ProcessIdentity {
        alive: true,
        start: start_secs(pid),
    }
}

#[cfg(unix)]
fn start_secs(pid: u32) -> Option<u64> {
    super::super::sessions::process_start_secs(pid)
}

/// Windows has a precise answer and no `ps`: `GetProcessTimes` reports the
/// creation time of the process a handle refers to, as a FILETIME in 100ns
/// ticks since 1601-01-01. Converted to epoch seconds so both platforms
/// answer in the same unit and [`classify`] needs no cfg split of its own.
#[cfg(windows)]
fn start_secs(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// 1601-01-01 to 1970-01-01, in seconds.
    const EPOCH_DELTA_SECS: u64 = 11_644_473_600;
    const TICKS_PER_SEC: u64 = 10_000_000;

    // SAFETY: `OpenProcess` returns a null handle on failure, which is
    // checked before use; every out-parameter below is a fully initialised
    // local this call owns, and the handle is closed on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let ticks =
            ((creation.dwHighDateTime as u64) << 32) | u64::from(creation.dwLowDateTime);
        Some((ticks / TICKS_PER_SEC).saturating_sub(EPOCH_DELTA_SECS))
    }
}

#[cfg(not(any(unix, windows)))]
fn start_secs(_pid: u32) -> Option<u64> {
    None
}

/// `<state>/runtime` -- a sibling of `sessions()`/`groups()`, holding the
/// runtime service's own durable state rather than any one session's.
pub fn runtime_dir(state: &StateDir) -> PathBuf {
    state.root().join("runtime")
}

/// `<state>/runtime/<name>.json`. The name is slug-sanitised through the same
/// helper every other path-from-a-name in this codebase uses, so no namespace
/// name can point the record outside this directory.
pub fn record_path(state: &StateDir, name: &str) -> PathBuf {
    runtime_dir(state).join(format!("{}.json", state::provider_slug(name)))
}

pub fn write(state: &StateDir, record: &Namespace) -> CtxResult<()> {
    state::create_private_dir_all(&runtime_dir(state))?;
    let body = serde_json::to_string_pretty(record)?;
    state::write_private(&record_path(state, &record.name), &body)?;
    Ok(())
}

/// `None` for absent, unreadable or malformed -- a question, never a cleanup.
pub fn read(state: &StateDir, name: &str) -> Option<Namespace> {
    read_path(&record_path(state, name))
}

fn read_path(path: &Path) -> Option<Namespace> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Every namespace record on disk, sorted by name.
pub fn list(state: &StateDir) -> Vec<Namespace> {
    let Ok(entries) = std::fs::read_dir(runtime_dir(state)) else {
        return Vec::new();
    };
    let mut found: Vec<Namespace> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| read_path(&entry.path()))
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Removes a namespace record. Used when a service shuts down cleanly, and
/// when one takes over a record it has just classified as replaceable.
pub fn remove(state: &StateDir, name: &str) {
    let _ = std::fs::remove_file(record_path(state, name));
}

/// Stamps `last_client_at` without rewriting anything else. Best-effort: a
/// heartbeat that cannot be written is not a reason to refuse a client.
pub fn touch(state: &StateDir, name: &str, now: u64) {
    if let Some(mut record) = read(state, name) {
        record.last_client_at = now;
        let _ = write(state, &record);
    }
}

/// A fresh record for a service starting now. `instance` is a v4 uuid: it is
/// never derived from the pid, the name or the endpoint, so two services that
/// happen to share any of those still get different identities.
pub fn new_record(name: &str, endpoint: &str, now: u64, history: bool) -> Namespace {
    let pid = std::process::id();
    Namespace {
        name: name.to_string(),
        owner: Owner {
            pid,
            start: start_secs(pid),
            user: std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .ok(),
        },
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: super::super::api::wire::PROTOCOL_VERSION,
        endpoint: endpoint.to_string(),
        created_at: now,
        last_client_at: now,
        instance: uuid::Uuid::new_v4().to_string(),
        history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: u32, start: Option<u64>, last_client_at: u64) -> Namespace {
        Namespace {
            name: DEFAULT_NAMESPACE.to_string(),
            owner: Owner {
                pid,
                start,
                user: Some("op".to_string()),
            },
            version: "4.7.0".to_string(),
            protocol: 1,
            endpoint: "<state>/s/api.sock".to_string(),
            created_at: 1_000,
            last_client_at,
            instance: "11111111-1111-4111-8111-111111111111".to_string(),
            history: false,
        }
    }

    fn probe(alive: bool, start: Option<u64>) -> impl Fn(u32) -> ProcessIdentity {
        move |_| ProcessIdentity { alive, start }
    }

    /// The record carries every field issue #352 names, and a round trip
    /// keeps them -- pinned here rather than left to the derive, because a
    /// client reading someone else's runtime has only this file to go on.
    #[test]
    fn a_namespace_record_carries_owner_version_endpoint_and_both_timestamps() {
        let record = record(4242, Some(900), 1_500);
        let value = serde_json::to_value(&record).expect("serialize");
        for key in [
            "owner",
            "version",
            "protocol",
            "endpoint",
            "created_at",
            "last_client_at",
            "instance",
        ] {
            assert!(value.get(key).is_some(), "missing {key}: {value}");
        }
        assert_eq!(value["owner"]["pid"], serde_json::json!(4242));
        let back: Namespace = serde_json::from_value(value).expect("round trip");
        assert_eq!(back, record);
    }

    /// The headline rule: a live pid whose start identity does not match the
    /// record is a RECYCLED pid, not a live runtime. Without the start
    /// identity this is exactly the case a pid-only check answers wrongly --
    /// and answering it wrongly means refusing to start a runtime because
    /// some unrelated program inherited the number.
    #[test]
    fn a_recycled_pid_is_stale_even_though_the_process_is_alive() {
        let record = record(4242, Some(1_000), 1_000);
        let verdict = classify(&record, &probe(true, Some(90_000)), 90_100, 120);
        assert_eq!(verdict, Liveness::Recycled);
        assert!(verdict.is_replaceable());
    }

    #[test]
    fn the_same_process_read_twice_is_live_despite_clock_noise() {
        let record = record(4242, Some(1_000), 1_000);
        // Inside the tolerance: an NTP step, not a different process.
        assert_eq!(
            classify(&record, &probe(true, Some(1_000 + 299)), 1_100, 120),
            Liveness::Live
        );
        assert!(!classify(&record, &probe(true, Some(1_005)), 1_100, 120).is_replaceable());
    }

    #[test]
    fn a_dead_pid_is_gone_without_consulting_a_start_time_at_all() {
        let record = record(4242, Some(1_000), 1_000);
        assert_eq!(
            classify(&record, &probe(false, Some(1_000)), 1_100, 120),
            Liveness::Gone
        );
    }

    /// Missing identity on either side is "cannot tell", and a quiet
    /// heartbeat then reads as unverified -- never as live (which would let a
    /// crashed runtime block a new one forever) and never as gone (which
    /// would let a second runtime stomp a live one).
    #[test]
    fn a_pid_with_no_start_identity_is_unverified_rather_than_guessed_at() {
        let unstamped = record(4242, None, 1_000);
        assert_eq!(
            classify(&unstamped, &probe(true, Some(1_000)), 1_000_000, 120),
            Liveness::Unverified
        );
        let unreadable = record(4242, Some(1_000), 1_000);
        assert_eq!(
            classify(&unreadable, &probe(true, None), 1_000_000, 120),
            Liveness::Unverified
        );
        // A fresh heartbeat is still enough to call it live while the
        // operator's own client is actively talking to it.
        assert_eq!(
            classify(&unreadable, &probe(true, None), 1_060, 120),
            Liveness::Live
        );
    }

    /// Two services started in the same directory under the same name get
    /// different instance ids, so neither can be mistaken for the other's
    /// successor -- the anchor for "never reuse another process's session
    /// identity".
    #[test]
    fn every_service_start_mints_a_fresh_instance_identity() {
        let first = new_record(DEFAULT_NAMESPACE, "e", 1_000, false);
        let second = new_record(DEFAULT_NAMESPACE, "e", 1_000, false);
        assert_ne!(first.instance, second.instance);
        assert_eq!(first.owner.pid, second.owner.pid, "same process, on purpose");
    }

    /// This process is genuinely alive and genuinely has a start identity on
    /// both supported platforms -- the one assertion that exercises the real
    /// probe rather than an injected one.
    #[test]
    fn the_real_probe_reports_this_process_as_live_with_a_start_identity() {
        let identity = process_identity(std::process::id());
        assert!(identity.alive);
        assert!(
            identity.start.is_some(),
            "unix reads it from ps, windows from GetProcessTimes"
        );
        let record = new_record(DEFAULT_NAMESPACE, "e", state::now_secs(), false);
        assert_eq!(
            classify(&record, &process_identity, state::now_secs(), 120),
            Liveness::Live,
            "a record this process just wrote must classify as its own"
        );
    }

    #[test]
    fn a_record_round_trips_through_the_state_directory_and_is_listed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read(&state, DEFAULT_NAMESPACE).is_none());
        assert!(list(&state).is_empty());

        let record = record(7, Some(1), 2);
        write(&state, &record).expect("write");
        assert_eq!(read(&state, DEFAULT_NAMESPACE).as_ref(), Some(&record));
        assert_eq!(list(&state), vec![record]);

        touch(&state, DEFAULT_NAMESPACE, 99);
        assert_eq!(
            read(&state, DEFAULT_NAMESPACE).expect("record").last_client_at,
            99
        );

        remove(&state, DEFAULT_NAMESPACE);
        assert!(read(&state, DEFAULT_NAMESPACE).is_none());
    }

    /// A namespace name is a file name, so it goes through the same slug
    /// sanitiser every other name-derived path does: nothing a caller types
    /// can escape `<state>/runtime`.
    #[test]
    fn a_namespace_name_can_never_name_a_path_outside_the_runtime_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let path = record_path(&state, "../../etc/passwd");
        assert!(
            path.starts_with(runtime_dir(&state)),
            "escaped to {}",
            path.display()
        );
        assert!(!path.to_string_lossy().contains(".."));
    }
}
