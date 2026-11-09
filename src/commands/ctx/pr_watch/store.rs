//! Watch records: `<state>/pr-watch/<session short>/<hash(url)>.json`, one
//! per watched PR/MR, written privately and atomically. A sibling `.lock`
//! file serialises pollers: whoever cannot take it skips this watch.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::WatchState;
use super::parse::Failure;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{self, FileLock, StateDir};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastError {
    pub kind: Failure,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRecord {
    pub url: String,
    /// Owning session's short id.
    pub owner: String,
    /// Checkout the watch was registered from: where CI config is looked for.
    pub repo: PathBuf,
    /// Mailbox the wakes land in.
    pub repo_slug: String,
    /// Harness of the owning session, for mail addressing.
    pub agent: String,
    /// Cached CLI identity, so the viewer is read once per watch.
    pub viewer: Option<String>,
    pub state: WatchState,
    pub registered_at: u64,
    pub last_polled: u64,
    /// Wakes mailed so far; bumped and persisted before each mail is stored.
    pub wake_seq: u64,
    pub last_error: Option<LastError>,
    pub consecutive_failures: u32,
    pub not_found: u32,
    /// Why polling stopped (`ended`, `wake_cap`, `not_found`, `failures`); stopped watches are not polled.
    pub stopped: Option<String>,
}

fn key(url: &str) -> String {
    state::hex_sha256(url.as_bytes()).chars().take(16).collect()
}

fn dir(state: &StateDir, owner: &str) -> PathBuf {
    state.pr_watch().join(owner)
}

fn path(state: &StateDir, owner: &str, url: &str) -> PathBuf {
    dir(state, owner).join(format!("{}.json", key(url)))
}

pub(super) fn save(state: &StateDir, record: &WatchRecord) -> CtxResult<()> {
    state::create_private_dir_all(&dir(state, &record.owner))?;
    let text = serde_json::to_string_pretty(record)?;
    state::write_private(&path(state, &record.owner, &record.url), &text)?;
    Ok(())
}

pub(super) fn load(state: &StateDir, owner: &str, url: &str) -> Option<WatchRecord> {
    serde_json::from_str(&std::fs::read_to_string(path(state, owner, url)).ok()?).ok()
}

/// Every readable record of `owner`, oldest registration first.
pub(super) fn list(state: &StateDir, owner: &str) -> Vec<WatchRecord> {
    let Ok(entries) = std::fs::read_dir(dir(state, owner)) else {
        return Vec::new();
    };
    let mut records: Vec<WatchRecord> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok())
        .collect();
    records.sort_by(|a, b| (a.registered_at, &a.url).cmp(&(b.registered_at, &b.url)));
    records
}

/// True when a record existed.
pub(super) fn remove(state: &StateDir, owner: &str, url: &str) -> bool {
    std::fs::remove_file(path(state, owner, url)).is_ok()
}

/// Non-blocking per-watch lock; an error means another poller holds it.
pub(super) fn try_lock(state: &StateDir, owner: &str, url: &str) -> CtxResult<FileLock> {
    state::create_private_dir_all(&dir(state, owner))?;
    state::try_acquire_lock(&dir(state, owner).join(format!("{}.lock", key(url))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(url: &str, registered_at: u64) -> WatchRecord {
        WatchRecord {
            url: url.into(),
            owner: "abcd1234".into(),
            repo: PathBuf::from("/work/repo"),
            repo_slug: "-work-repo".into(),
            agent: "claude".into(),
            viewer: Some("me".into()),
            state: WatchState::new(registered_at),
            registered_at,
            last_polled: 0,
            wake_seq: 3,
            last_error: Some(LastError {
                kind: Failure::RateLimited,
                at: 9,
            }),
            consecutive_failures: 1,
            not_found: 0,
            stopped: None,
        }
    }

    #[test]
    fn a_record_round_trips_and_lists_oldest_first() {
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(root.path().to_path_buf());
        let later = record("https://github.com/a/b/pull/2", 20);
        let earlier = record("https://github.com/a/b/pull/1", 10);
        save(&state, &later).expect("save");
        save(&state, &earlier).expect("save");
        assert_eq!(load(&state, "abcd1234", &later.url), Some(later.clone()));
        assert_eq!(list(&state, "abcd1234"), vec![earlier.clone(), later]);
        assert!(list(&state, "other999").is_empty());
        assert!(remove(&state, "abcd1234", &earlier.url));
        assert!(!remove(&state, "abcd1234", &earlier.url));
        assert_eq!(load(&state, "abcd1234", &earlier.url), None);
    }

    #[cfg(unix)]
    #[test]
    fn records_are_written_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(root.path().to_path_buf());
        let rec = record("https://github.com/a/b/pull/1", 10);
        save(&state, &rec).expect("save");
        let mode = std::fs::metadata(path(&state, &rec.owner, &rec.url))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode {mode:o}");
    }

    #[test]
    fn the_try_lock_admits_one_poller_per_watch() {
        let root = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(root.path().to_path_buf());
        let url = "https://github.com/a/b/pull/1";
        let first = try_lock(&state, "abcd1234", url).expect("first poller");
        assert!(
            try_lock(&state, "abcd1234", url).is_err(),
            "second is refused"
        );
        assert!(
            try_lock(&state, "abcd1234", "https://github.com/a/b/pull/2").is_ok(),
            "another watch is independent"
        );
        drop(first);
        assert!(try_lock(&state, "abcd1234", url).is_ok(), "free again");
    }
}
