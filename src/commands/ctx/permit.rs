//! The machine-wide heavy-OPERATION budget (issue #155, replacing issue
//! #133's heavy-*worker* count).
//!
//! Heavy permits cover actual command lifetimes, so idle workers hold no budget (#155).
//! Classification is pure; filesystem and clock access belong in acquisition and sweeping.
//!
//! Numbered `slot-<n>.json` files use atomic `create_new` claims to enforce the cap.
//! Writer permits live in `<state>/permits/writers/` for the worker's whole lifetime;
//! per-tree claims enforce exclusive checkout access even when `max_writers` is zero (#267, #338).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::sessions;
use super::state::{self, StateDir, create_new_private};

/// Creation and writing are separate syscalls: abort can leave a partial slot behind.
/// A grace window recovers those slots without sweeping writes still in progress.
const UNPARSEABLE_SLOT_GRACE_SECS: u64 = 5;

/// Only lost claims need retries; `create_new` resolves ordinary contention in one pass.
const CLAIM_VERIFY_ATTEMPTS: u32 = 3;

/// Operator patterns may only add to this mandatory set, never remove its protections.
/// Cheap commands such as `cargo fmt` must not occupy the heavy-operation budget.
pub const BUILTIN_HEAVY_PATTERNS: &[&str] = &[
    "cargo build*",
    "cargo test*",
    "cargo nextest*",
    "cargo clippy*",
    "cargo package*",
    "cargo publish*",
    // Hyperframes rendering runs Chrome and FFmpeg; its cheap subcommands need no permit.
    // Bound ` render` after the version wildcard so paths such as `renders/x` stay light.
    "npx --yes hyperframes@* render",
    "npx --yes hyperframes@* render *",
    "npx hyperframes render*",
    "hyperframes render*",
];

/// Use the safety matcher so shell wrappers, chains and substitutions cannot evade the budget.
pub fn is_heavy(command: &str, extra_patterns: &[String]) -> bool {
    super::safety::normalize_segments(command)
        .iter()
        .any(|candidate| {
            BUILTIN_HEAVY_PATTERNS
                .iter()
                .copied()
                .chain(extra_patterns.iter().map(String::as_str))
                .any(|pattern| super::safety::glob_match(pattern, candidate))
        })
}

/// Separate pools keep worker lifetimes from consuming command slots (#133, #155, #267).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermitKind {
    #[default]
    Heavy,
    Writer,
}

/// Defaults to writing: an unnecessary permit is safer than silently dropping edits (#267).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerMode {
    ReadOnly,
    #[default]
    Writing,
}

/// Held permit details let status and wait messages identify the budget holder (#162).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermitRecord {
    pub pid: u32,
    #[serde(default)]
    pub pid_start_time: Option<u64>,
    /// Keeps the slot live if the child outlives its parent; absent until spawn or in legacy records.
    #[serde(default)]
    pub child_pid: Option<u32>,
    #[serde(default)]
    pub child_start_time: Option<u64>,
    pub label: String,
    pub acquired_at: u64,
    /// Missing kinds deserialize as `Heavy` for compatibility with legacy records (#267).
    #[serde(default)]
    pub kind: PermitKind,
    /// Compare canonical writer paths through [`tree_key`], never raw `PathBuf` equality:
    /// case aliases must not admit two writers; heavy records carry `None` (#267).
    #[serde(default)]
    pub tree: Option<PathBuf>,
}

/// Numbered files under `<state>/permits/` provide atomic contention targets for budget slots.
fn permits_dir(state: &StateDir) -> PathBuf {
    state.root().join("permits")
}

fn slot_path(dir: &Path, slot: usize) -> PathBuf {
    dir.join(format!("slot-{slot}.json"))
}

/// `Drop` is best-effort and abort skips it, so dead-owner sweeping must recover abandoned slots.
#[derive(Debug)]
pub struct HeavyPermit {
    path: PathBuf,
    /// Writer tree claim, acquired before the pool slot and released with it; absent for heavy permits.
    tree_claim: Option<PathBuf>,
    /// Canonical checkout proving the broker's write is backed by this guard; absent for heavy permits.
    tree: Option<PathBuf>,
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(tree_claim) = &self.tree_claim {
            let _ = std::fs::remove_file(tree_claim);
        }
    }
}

impl HeavyPermit {
    pub fn writer_tree(&self) -> Option<&Path> {
        self.tree.as_deref()
    }

    /// Records child liveness in both the slot and tree claim so either survives parent exit.
    /// I/O and serialization failures are silent and leave the existing record unchanged.
    pub fn set_child_pid(&self, child_pid: u32) {
        Self::write_child_pid(&self.path, child_pid);
        if let Some(tree_claim) = &self.tree_claim {
            Self::write_child_pid(tree_claim, child_pid);
        }
    }

    fn write_child_pid(path: &Path, child_pid: u32) {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(mut record) = serde_json::from_str::<PermitRecord>(&contents) else {
            return;
        };
        record.child_pid = Some(child_pid);
        record.child_start_time = sessions::process_start_secs(child_pid);
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            // Atomic replacement prevents concurrent readers from sweeping a truncated record
            // while its child is still running.
            let _ = super::state::write_private(path, &json);
        }
    }
}

/// Both sweeps must retain a permit while either parent or child lives; parent exit alone cannot free it.
pub(crate) fn permit_record_is_alive(record: &PermitRecord) -> bool {
    permit_record_is_alive_with(
        record,
        sessions::process_start_secs(record.pid),
        record.child_pid.and_then(sessions::process_start_secs),
    )
}

fn permit_record_is_alive_with(
    record: &PermitRecord,
    parent_start: Option<u64>,
    child_start: Option<u64>,
) -> bool {
    let alive = |pid, recorded, current| {
        sessions::is_alive(pid) && !sessions::start_time_disambiguates_dead(recorded, current)
    };
    alive(record.pid, record.pid_start_time, parent_start)
        || record
            .child_pid
            .is_some_and(|pid| alive(pid, record.child_start_time, child_start))
}

fn is_stale(modified: SystemTime, now: SystemTime, grace_secs: u64) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age.as_secs() > grace_secs,
        // Future mtimes indicate clock skew, not stale claims.
        Err(_) => false,
    }
}

/// I/O or clock errors preserve the file: uncertain age is not grounds for deletion.
fn slot_file_is_stale(path: &Path, grace_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    is_stale(modified, SystemTime::now(), grace_secs)
}

/// Lists holders for diagnostics and sweeps dead owners so crashes cannot wedge the budget (#162).
/// Unreadable directories and unreadable or malformed files do not fail the listing.
pub fn live_records(state: &StateDir) -> Vec<PermitRecord> {
    live_records_in(&permits_dir(state))
}

/// Shared sweep keeps heavy and writer pools consistent (#267).
fn live_records_in(dir: &Path) -> Vec<PermitRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<PermitRecord>(&contents) else {
            // Allow in-progress writes to finish before removing crash-orphaned partial files.
            if slot_file_is_stale(&path, UNPARSEABLE_SLOT_GRACE_SECS) {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        };
        let alive = permit_record_is_alive(&record);
        if alive {
            found.push(record);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    found
}

pub fn live_count(state: &StateDir) -> usize {
    live_records(state).len()
}

/// Atomic slot creation, never a live count, decides admission; write failure safely grants no permit.
pub fn acquire(state: &StateDir, limit: usize, label: &str) -> Option<HeavyPermit> {
    let record = PermitRecord {
        pid: std::process::id(),
        pid_start_time: sessions::process_start_secs(std::process::id()),
        child_start_time: None,
        child_pid: None,
        label: label.to_string(),
        acquired_at: state::now_secs(),
        kind: PermitKind::Heavy,
        tree: None,
    };
    acquire_record(&permits_dir(state), limit, record)
}

/// Both pools share claim verification so a sweep cannot grant a phantom permit;
/// `record.pid` must be the acquiring caller's identity (#267).
fn acquire_record(dir: &Path, limit: usize, record: PermitRecord) -> Option<HeavyPermit> {
    if limit == 0 {
        return None;
    }
    state::create_private_dir_all(dir).ok()?;

    // A stale count may refuse a free slot but must never grant one;
    // only atomic slot creation below enforces the limit.
    if live_records_in(dir).len() >= limit {
        return None;
    }

    let own_pid = record.pid;
    let json = serde_json::to_string_pretty(&record).ok()?;

    // A concurrent dead-owner sweep can delete a replacement claim after reading its predecessor.
    // Verify the written record and retry lost claims before granting a permit.
    for _ in 0..CLAIM_VERIFY_ATTEMPTS {
        let path = claim_any_slot(dir, limit, &json)?;
        if claim_is_verified(&path, own_pid) {
            return Some(HeavyPermit {
                path,
                tree_claim: None,
                tree: record.tree.clone(),
            });
        }
    }
    None
}

/// Separate writer directory keeps heavy-pool listings independent (#267).
fn writer_permits_dir(state: &StateDir) -> PathBuf {
    permits_dir(state).join("writers")
}

pub fn live_writer_records(state: &StateDir) -> Vec<PermitRecord> {
    live_records_in(&writer_permits_dir(state))
}

/// Reconcile dry runs must never mutate the filesystem, even when reporting dead owners (#720).
pub(crate) fn dead_records(state: &StateDir) -> Vec<PermitRecord> {
    dead_records_in(&permits_dir(state))
        .into_iter()
        .chain(dead_records_in(&writer_permits_dir(state)))
        .collect()
}

/// Dry runs must leave even malformed records untouched.
fn dead_records_in(dir: &Path) -> Vec<PermitRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let contents = std::fs::read_to_string(&path).ok()?;
            let record: PermitRecord = serde_json::from_str(&contents).ok()?;
            (!permit_record_is_alive(&record)).then_some(record)
        })
        .collect()
}

/// Callers must canonicalize first: this comparison key never probes the filesystem.
/// Windows/macOS case aliases must not admit two writers to one checkout (#267).
pub fn tree_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if cfg!(any(windows, target_os = "macos")) {
        raw.to_lowercase()
    } else {
        raw.into_owned()
    }
}

/// Identifies the holder on tree contention; tree and pool refusals are retryable (#162).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterRefusal {
    TreeBusy {
        holder_label: String,
    },
    PoolExhausted,
    /// Seat ownership failure is not retryable as contention (#488).
    /// Explicit fences reject superseded and uncommitted generations; env fences reject supersession only.
    StaleSeat {
        stale: super::seat::StaleGeneration,
    },
}

/// Runtime-supplied generations require strict checks; only env fallback permits a prepared successor's launch (#488).
#[derive(Debug, Clone, Copy)]
pub struct SeatFence<'a> {
    pub short: &'a str,
    pub generation: u64,
}

/// Name holders and their trees so cross-repository contention is diagnosable without another status call (#267, #338).
pub(crate) fn describe_writer_refusal(
    refusal: &WriterRefusal,
    state: &StateDir,
    max_writers: usize,
    tree: &Path,
) -> String {
    match refusal {
        WriterRefusal::TreeBusy { holder_label } => format!(
            "writer-busy: another writing worker already holds {} ({holder_label}); retry once \
             it finishes, or pass --worktree for an isolated checkout",
            tree.display()
        ),
        WriterRefusal::PoolExhausted => {
            let holders = live_writer_records(state);
            let mut description = format!(
                "writer-busy: the writer-permit pool ({} of {} in use) is full; raise \
                 supervise.max_writers or ZIRV_CTX_SUPERVISE_MAX_WRITERS (0 lifts the \
                 machine-wide cap), or retry once a writer finishes",
                holders.len(),
                max_writers
            );
            for holder in holders {
                let holder_tree = holder
                    .tree
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(unknown tree)".to_string());
                description.push_str(&format!(
                    "\n  pid {} -- {} -- {holder_tree}",
                    holder.pid, holder.label
                ));
            }
            description
        }
        WriterRefusal::StaleSeat { stale } => format!(
            "writer-refused: {stale}. This session does not hold the orchestrator seat, so it may \
             not take a writer lease on {}; retrying will not change that.",
            tree.display()
        ),
    }
}

/// Separate tree-claim directory excludes claims from the non-recursive pool-slot listing.
fn tree_claims_dir(state: &StateDir) -> PathBuf {
    writer_permits_dir(state).join("trees")
}

/// Raw paths contain separators, Windows drive colons and unbounded lengths, so claims use hashes.
/// `DefaultHasher` is not stable across toolchains; contenders must run the same compiled binary.
fn tree_claim_hash(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn tree_claim_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("tree-{}.json", tree_claim_hash(key)))
}

/// Without a shared lock, exclusive admission cannot be proved, so lock failure must refuse (#728).
fn lock_tree_claim(dir: &Path) -> Result<super::state::FileLock, WriterRefusal> {
    let path = dir.join(".lock");
    super::state::acquire_lock(&path).map_err(|_| WriterRefusal::PoolExhausted)
}

/// Atomically claims tree exclusivity before pool admission to prevent concurrent writers sharing a tree.
fn claim_tree(dir: &Path, key: &str, record: &PermitRecord) -> Result<PathBuf, WriterRefusal> {
    let _ = state::create_private_dir_all(dir);
    let _lock = lock_tree_claim(dir)?;
    let path = tree_claim_path(dir, key);
    let Ok(json) = serde_json::to_string_pretty(record) else {
        return Err(WriterRefusal::PoolExhausted);
    };
    for _ in 0..CLAIM_VERIFY_ATTEMPTS {
        match create_new_private(&path, &json) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrently released claim may disappear before this read; retry it.
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    match serde_json::from_str::<PermitRecord>(&contents) {
                        Ok(existing) => {
                            let alive = permit_record_is_alive(&existing);
                            if alive {
                                return Err(WriterRefusal::TreeBusy {
                                    holder_label: existing.label,
                                });
                            }
                            let _ = std::fs::remove_file(&path);
                        }
                        Err(_) => {
                            // Preserve fresh partial writes until the grace window expires.
                            if slot_file_is_stale(&path, UNPARSEABLE_SLOT_GRACE_SECS) {
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                }
            }
            // A transient I/O failure need not defeat later attempts at exclusive creation.
            Err(_) => {}
        }
    }
    // Unconfirmed tree ownership must refuse admission to preserve exclusivity.
    Err(WriterRefusal::TreeBusy {
        holder_label: "(unknown -- contended claim)".to_string(),
    })
}

/// A writer must own its tree for its whole lifetime, even when `limit == 0` removes the pool cap.
/// Uncapped writers still need holder records for diagnostics; failed admission must release the tree (#267).
pub fn acquire_writer(
    state: &StateDir,
    limit: usize,
    label: &str,
    tree: &Path,
    fence: Option<SeatFence<'_>>,
) -> Result<HeavyPermit, WriterRefusal> {
    // Writer leases mutate service state, so explicit fences require committed ownership.
    // Rollover exports a prepared generation before commit: env fences must allow its launch; seatless callers stay unfenced (#488).
    let _generation = match fence {
        Some(fence) => match super::seat::lock_generation(state, fence.short, fence.generation) {
            Ok(guard) => guard,
            Err(error) => {
                if let Some(stale) = error.downcast_ref::<super::seat::StaleGeneration>() {
                    return Err(WriterRefusal::StaleSeat {
                        stale: stale.clone(),
                    });
                }
                return Err(WriterRefusal::PoolExhausted);
            }
        },
        None => {
            if let Err(stale) = super::seat::guard_from_env(state) {
                return Err(WriterRefusal::StaleSeat { stale });
            }
            None
        }
    };
    let dir = writer_permits_dir(state);
    let key = tree_key(tree);
    let record = PermitRecord {
        pid: std::process::id(),
        pid_start_time: sessions::process_start_secs(std::process::id()),
        child_start_time: None,
        child_pid: None,
        label: label.to_string(),
        acquired_at: state::now_secs(),
        kind: PermitKind::Writer,
        tree: Some(tree.to_path_buf()),
    };
    let claim_path = claim_tree(&tree_claims_dir(state), &key, &record)?;
    let effective_limit = if limit == 0 { usize::MAX } else { limit };
    match acquire_record(&dir, effective_limit, record) {
        Some(mut permit) => {
            permit.tree_claim = Some(claim_path);
            Ok(permit)
        }
        None => {
            // A failed admission must not leave the tree blocked.
            let _ = std::fs::remove_file(&claim_path);
            Err(WriterRefusal::PoolExhausted)
        }
    }
}

/// Scans all slots so a lost claim can retry beyond its original index.
fn claim_any_slot(dir: &Path, limit: usize, json: &str) -> Option<PathBuf> {
    for slot in 0..limit {
        let path = slot_path(dir, slot);
        match create_new_private(&path, json) {
            Ok(()) => return Some(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // A transient error on one slot must not prevent trying the others.
            Err(_) => continue,
        }
    }
    None
}

/// Verifies the written owner so a lost or replaced claim cannot grant a phantom permit.
fn claim_is_verified(path: &Path, expected_pid: u32) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(record) = serde_json::from_str::<PermitRecord>(&contents) else {
        return false;
    };
    record.pid == expected_pid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycled_permit_owners_are_dead_but_legacy_claims_use_bare_pids() {
        let pid = std::process::id();
        // Omitted start-time fields model a claim written by an older binary.
        let mut record: PermitRecord = serde_json::from_value(serde_json::json!({
            "pid": pid, "label": "writer", "acquired_at": 0, "kind": "writer"
        }))
        .expect("legacy claim");
        let dead_pid = 2_000_000_000;
        assert!(!sessions::is_alive(dead_pid));
        for child_owner in [false, true] {
            record.pid = if child_owner { dead_pid } else { pid };
            record.child_pid = child_owner.then_some(pid);
            record.pid_start_time = None;
            record.child_start_time = None;
            assert!(permit_record_is_alive_with(&record, Some(4600), Some(4600)));
            if child_owner {
                record.child_start_time = Some(1000);
            } else {
                record.pid_start_time = Some(1000);
            }
            assert!(!permit_record_is_alive_with(
                &record,
                Some(4600),
                Some(4600)
            ));
            assert!(permit_record_is_alive_with(&record, Some(1000), Some(1000)));
            assert!(permit_record_is_alive_with(&record, None, None));
        }
    }

    fn write_orphan_permit(state: &StateDir, label: &str, pid: u32) {
        let dir = permits_dir(state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: label.to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        };
        let path = dir.join(format!("{pid}-{}.json", uuid::Uuid::new_v4()));
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        state::write_private(&path, &json).expect("write");
    }

    /// Issue #155, Phase 5(e): the budget must count WORK, not sessions. The
    /// old rule counted `Verb::Exec | Verb::Dash` records, so an idle worker
    /// consumed the whole default budget of 1 -- which meant one parked
    /// delegation blocked every subsequent one, and the orchestrator did the
    /// work itself on the expensive seat.
    /// A permit slot file is read by other processes while its owner may be
    /// updating the child pid into it, and `live_records_in` REMOVES a file
    /// it cannot parse once it is past the grace window -- so a truncating
    /// write here can free a slot whose heavy child is still running. The
    /// update must therefore never leave the file unreadable for an instant.
    #[test]
    fn updating_a_child_pid_never_leaves_a_slot_file_unreadable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("slot-0.json");
        let record = PermitRecord {
            pid: 4242,
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        };
        state::write_private(
            &path,
            &serde_json::to_string_pretty(&record).expect("serialize"),
        )
        .expect("write");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let path = path.clone();
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut unreadable = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(contents) = std::fs::read_to_string(&path)
                        && serde_json::from_str::<PermitRecord>(&contents).is_err()
                    {
                        unreadable += 1;
                    }
                }
                unreadable
            })
        };

        for child_pid in 1..600u32 {
            HeavyPermit::write_child_pid(&path, child_pid);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let unreadable = reader.join().expect("reader thread");

        assert_eq!(
            unreadable, 0,
            "a concurrent reader saw the slot file unparseable {unreadable} time(s)"
        );
        let final_record: PermitRecord =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read"))
                .expect("final parse");
        assert_eq!(final_record.child_pid, Some(599));
    }

    #[test]
    fn heavy_classification_is_about_the_command_not_the_session() {
        let none: Vec<String> = Vec::new();
        for heavy in [
            "cargo build",
            "cargo build --release",
            "cargo test --verbose -- --test-threads=1",
            "cargo nextest run --no-fail-fast",
            "cargo clippy --all-targets -- -D warnings",
            "cargo package",
            "cargo publish --dry-run",
        ] {
            assert!(is_heavy(heavy, &none), "{heavy} must hold a permit");
        }
        for light in [
            "git status",
            "cargo --version",
            "cargo fmt -- --check",
            "ls",
            "rg TODO src/",
            "echo cargo build",
        ] {
            assert!(!is_heavy(light, &none), "{light} must not hold a permit");
        }
    }

    /// A Hyperframes render holds a permit like a cargo build; the cheap
    /// preflight/inspection/audio commands it also exposes must not.
    #[test]
    fn hyperframes_render_is_heavy_but_its_other_commands_are_not() {
        let none: Vec<String> = Vec::new();
        for heavy in [
            "npx --yes hyperframes@0.8.80 render --format gif --fps 15 --output renders/demo.gif",
            "npx hyperframes render",
            "hyperframes render -o a.mp4",
            "HYPERFRAMES_SKIP_SKILLS=1 npx --yes hyperframes@0.8.80 render --output renders/demo.mp4",
        ] {
            assert!(is_heavy(heavy, &none), "{heavy} must hold a permit");
        }
        for light in [
            "npx hyperframes lint",
            "npx --yes hyperframes@0.8.80 snapshot",
            "npx --yes hyperframes@0.8.80 tts \"hi\" --output vo.wav",
            "npx --yes hyperframes@0.8.80 check --output renders/check.json",
        ] {
            assert!(!is_heavy(light, &none), "{light} must not hold a permit");
        }
    }

    /// Operator patterns ADD to the built-in set; they never replace it. A
    /// repo layer may only add, which is narrowing.
    #[test]
    fn configured_patterns_extend_the_builtin_set() {
        let extra = vec!["npm run build*".to_string()];
        assert!(is_heavy("npm run build --workspaces", &extra));
        assert!(is_heavy("cargo build", &extra), "built-ins still apply");
        assert!(!is_heavy("npm run lint", &extra));
    }

    /// The permit itself: bounded, released on drop, and never held by an
    /// idle process.
    #[test]
    fn a_permit_is_bounded_and_released_on_drop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let first = acquire(&state, 1, "cargo build").expect("the first permit is granted");
        assert_eq!(live_count(&state), 1);
        assert!(
            acquire(&state, 1, "cargo nextest run").is_none(),
            "the budget of 1 must refuse a second concurrent heavy operation"
        );

        drop(first);
        assert_eq!(live_count(&state), 0);
        assert!(
            acquire(&state, 1, "cargo build").is_some(),
            "the slot is free again"
        );
    }

    /// Finding B1: acquisition used to be count-then-create with no lock, so
    /// two racing callers could both observe a free slot and both acquire,
    /// exceeding `limit`. Races `threads` real OS threads against a budget of
    /// 1 with a `Barrier` to line them up as close to simultaneously as this
    /// process can manage, and asserts the OUTCOME rather than the timing:
    /// however the race actually interleaves, `create_new`'s own atomicity
    /// must mean exactly one acquisition ever succeeds -- never zero (the old
    /// code could never grant none when a slot was free) and never more than
    /// one (the bug this test exists to catch).
    #[test]
    fn concurrent_acquisitions_never_exceed_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        const THREADS: usize = 16;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));

        // Held for the whole race so every granted permit is still live when
        // counted -- an ungated `Drop` racing the count below would make a
        // momentarily-too-low reading look like a pass for the wrong reason.
        let held: Vec<Option<HeavyPermit>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let state = state.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        acquire(&state, 1, &format!("racer-{i}"))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("racer thread must not panic"))
                .collect()
        });

        let granted = held.iter().filter(|permit| permit.is_some()).count();
        assert_eq!(
            granted, 1,
            "a budget of 1 must grant exactly one permit even when {THREADS} threads race for it"
        );
    }

    /// A permit whose owning process is gone must not wedge the budget
    /// forever -- the same dead-owner sweep `sessions::list` already performs
    /// for session records, and `dash`'s `owner.pid` sweep for request dirs.
    #[test]
    fn a_permit_left_by_a_dead_process_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dead_pid = crate::commands::ctx::testenv::dead_pid();
        write_orphan_permit(&state, "cargo build", dead_pid);
        assert_eq!(
            live_count(&state),
            0,
            "a dead owner's permit does not count"
        );
        assert!(acquire(&state, 1, "cargo build").is_some());
    }

    /// Issue #720 (the state-reconcile pass): `dead_records` reports the
    /// SAME dead-owner permit `live_records`'s own sweep would remove, but
    /// leaves it on disk -- the read-only counterpart a `--dry-run` reconcile
    /// pass needs. A live-owned permit is never reported, no matter how it
    /// looks otherwise.
    #[test]
    fn dead_records_reports_a_dead_owner_without_removing_it_and_ignores_a_live_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dead_pid = crate::commands::ctx::testenv::dead_pid();
        // `acquire` itself sweeps dead owners as a side effect (its own doc
        // comment), so the live permit must be acquired FIRST -- otherwise
        // its own internal `live_records_in` call would remove the
        // dead-owner file below before `dead_records` ever saw it.
        let _live = acquire(&state, 2, "cargo test").expect("live permit granted");
        write_orphan_permit(&state, "cargo build", dead_pid);

        let dead = dead_records(&state);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].pid, dead_pid);

        // Untouched: the dead-owner file is still on disk, and a live sweep
        // still finds (and only then removes) it.
        assert_eq!(
            std::fs::read_dir(permits_dir(&state))
                .expect("read permits dir")
                .count(),
            2,
            "dead_records must not remove the dead-owner file"
        );
        assert_eq!(live_count(&state), 1, "the live permit is still counted");
    }

    /// Finding B5: `pid` on a `PermitRecord` names the script-runner
    /// (parent) process that called `acquire`, not the actual heavy child it
    /// goes on to spawn. If the parent dies first while the real heavy child
    /// is still running, the sweep must not free the slot just because the
    /// PARENT is gone -- it must also check `child_pid`. Simulates that
    /// exact shape: a dead recorded `pid` (the gone parent) alongside a
    /// `child_pid` of this very test process (guaranteed alive for the
    /// duration of the test).
    #[test]
    fn a_permit_stays_live_on_a_dead_parent_if_its_child_is_still_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dead_pid = crate::commands::ctx::testenv::dead_pid();
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid: dead_pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: Some(std::process::id()),
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        state::write_private(&slot_path(&dir, 0), &json).expect("write");

        assert_eq!(
            live_count(&state),
            1,
            "a live child must keep the slot even though the recorded parent pid is dead"
        );
    }

    /// `HeavyPermit::set_child_pid` is the only way `child_pid` is ever set
    /// in production (`Command::invoke`, once the real child is spawned) --
    /// proves it actually persists to the same file `live_records` reads
    /// back, not just to an in-memory copy.
    #[test]
    fn set_child_pid_persists_to_the_permit_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let permit = acquire(&state, 1, "cargo build").expect("permit granted");

        permit.set_child_pid(4242);

        let records = live_records(&state);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].child_pid,
            Some(4242),
            "the child pid must be readable back through live_records, not just held in memory"
        );
    }

    /// Issue #162: a refusal or a wait that cannot say WHO holds the budget
    /// is undiagnosable. `live_records` must report the label each holder
    /// was acquired with, not just how many there are.
    #[test]
    fn live_records_reports_each_holders_own_label() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let _held =
            acquire(&state, 1, "session ab12cd34: cargo nextest run").expect("permit granted");

        let records = live_records(&state);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].label, "session ab12cd34: cargo nextest run");
        assert_eq!(records[0].pid, std::process::id());
    }

    /// Pure arithmetic underlying finding 2a's sweep decision, with no file
    /// or real clock involved: strictly older than `grace_secs` is stale,
    /// exactly at it is not yet, and a `modified` time in the future (clock
    /// skew, not a crash-orphaned file) is never stale.
    #[test]
    fn is_stale_marks_only_strictly_past_the_grace_window() {
        let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert!(
            !is_stale(base, base + std::time::Duration::from_secs(5), 5),
            "exactly at the grace window is not yet stale"
        );
        assert!(
            is_stale(base, base + std::time::Duration::from_secs(6), 5),
            "one second past the grace window is stale"
        );
        assert!(
            !is_stale(base, base - std::time::Duration::from_secs(1), 5),
            "a modified time in the future must never be treated as stale"
        );
    }

    /// Finding 2a: a crash-orphaned slot file (`acquire`'s own `create_new`
    /// then `write_all` is two syscalls in a `panic = "abort"` binary) older
    /// than the grace window must be swept, freeing its slot index -- before
    /// this fix `live_records` skipped an unparseable file forever without
    /// ever removing it, permanently losing that slot.
    #[test]
    fn an_old_unparseable_slot_file_is_swept_and_the_slot_becomes_claimable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        std::fs::write(&path, "").expect("write empty (unparseable) slot file");

        let old =
            SystemTime::now() - std::time::Duration::from_secs(UNPARSEABLE_SLOT_GRACE_SECS + 5);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for mtime")
            .set_modified(old)
            .expect("backdate mtime");

        assert_eq!(
            live_count(&state),
            0,
            "an unparseable file must never count as a held permit"
        );
        assert!(
            !path.exists(),
            "a crash-orphaned slot file older than the grace window must be swept"
        );
        assert!(
            acquire(&state, 1, "cargo build").is_some(),
            "the swept slot index must become claimable again"
        );
    }

    /// The other half of finding 2a: an unparseable file with a fresh mtime
    /// (just written) must NOT be swept -- it may be a write genuinely still
    /// in progress, and sweeping it out from under an in-flight `acquire`
    /// would reintroduce a phantom-permit race of its own.
    #[test]
    fn a_fresh_unparseable_slot_file_is_not_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        std::fs::write(&path, "").expect("write empty (unparseable) slot file");

        assert_eq!(live_count(&state), 0, "still not a held permit");
        assert!(
            path.exists(),
            "a fresh unparseable file must not be swept -- it may be a write still in progress"
        );
    }

    /// Finding 2b: `claim_is_verified` is true only when the record actually
    /// on disk right now is the one this call itself wrote.
    #[test]
    fn claim_is_verified_true_for_a_freshly_written_own_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        let pid = std::process::id();
        let json = serde_json::to_string_pretty(&PermitRecord {
            pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        })
        .expect("serialize");
        create_new_private(&path, &json).expect("write");

        assert!(claim_is_verified(&path, pid));
    }

    /// Finding 2b: a claim whose file is gone by the time of the verify read
    /// -- exactly what a concurrent dead-owner sweep winning the race would
    /// leave behind -- must never verify.
    #[test]
    fn claim_is_verified_false_once_the_file_is_gone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0); // never written

        assert!(!claim_is_verified(&path, std::process::id()));
    }

    /// Finding 2b: a claim whose file holds a record this call never wrote
    /// (a different pid) must never verify either -- "wrong", not only
    /// "gone".
    #[test]
    fn claim_is_verified_false_when_the_record_names_a_different_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let path = slot_path(&dir, 0);
        let json = serde_json::to_string_pretty(&PermitRecord {
            pid: std::process::id().wrapping_add(1),
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "someone else's claim".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        })
        .expect("serialize");
        create_new_private(&path, &json).expect("write");

        assert!(!claim_is_verified(&path, std::process::id()));
    }

    /// Finding 2b: exercises `acquire`'s own verify-then-rescan retry at the
    /// level of its building blocks -- a claim whose file vanishes between
    /// the write and the verify (simulating the dead-owner sweep race
    /// `claim_is_verified`'s doc comment describes) must not be trusted, and
    /// the freed index must be claimable again on the very next scan.
    #[test]
    fn a_claim_whose_file_vanishes_before_verification_is_not_trusted_and_the_slot_reclaims() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let dir = permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let own_pid = std::process::id();
        let json = serde_json::to_string_pretty(&PermitRecord {
            pid: own_pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "cargo build".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Heavy,
            tree: None,
        })
        .expect("serialize");

        let claimed = claim_any_slot(&dir, 1, &json).expect("the only slot is free");
        // Simulate a concurrent dead-owner sweep winning the race between
        // the write inside `claim_any_slot` and this check.
        std::fs::remove_file(&claimed).expect("simulate the sweep");
        assert!(
            !claim_is_verified(&claimed, own_pid),
            "a vanished claim must never verify"
        );

        let reclaimed = claim_any_slot(&dir, 1, &json).expect("the freed slot claims again");
        assert_eq!(reclaimed, claimed, "the same (only) slot index is reused");
    }

    /// Issue #267: a `PermitRecord` written before the writer pool existed
    /// (no `kind`/`tree` fields at all) must still deserialise, and must
    /// read as the only kind there ever was.
    #[test]
    fn a_permit_record_written_before_writer_pools_existed_still_deserialises_as_heavy() {
        let old = r#"{"pid":123,"label":"cargo build","acquired_at":1700000000}"#;
        let record: PermitRecord = serde_json::from_str(old).expect("older records still parse");
        assert_eq!(record.kind, PermitKind::Heavy);
        assert_eq!(record.tree, None);
    }

    /// Issue #267: pure case-folding, no filesystem involved -- the same
    /// path spelled with different case is the same tree only on a
    /// case-insensitive filesystem (Windows/macOS).
    #[test]
    fn tree_key_case_folds_only_on_windows_and_macos() {
        let a = tree_key(Path::new("/Repo/Foo"));
        let b = tree_key(Path::new("/repo/foo"));
        if cfg!(any(windows, target_os = "macos")) {
            assert_eq!(a, b, "case must be folded on this platform");
        } else {
            assert_ne!(a, b, "case must be preserved on this platform");
        }
    }

    /// Design section 3: a second `writing` worker into a tree that already
    /// has a live writer is refused, even when the pool itself has room for
    /// more (`limit` of 2 here) -- tree exclusivity is a separate rule from
    /// the bounded pool, not merely a side effect of a pool of 1.
    #[test]
    fn a_second_writer_in_the_same_tree_is_refused_while_the_first_is_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let _held =
            acquire_writer(&state, 2, "worker-a", &tree, None).expect("first writer granted");
        let err = acquire_writer(&state, 2, "worker-b", &tree, None)
            .expect_err("a second writer in the same tree must be refused");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            }
        );
    }

    /// Issue #488 (review finding 1): a writer lease is a mutable service
    /// operation, so it is fenced on the seat generation exactly as
    /// `delegation::delegate` and `coordinator::update_fenced` are -- a
    /// superseded predecessor AND an uncommitted successor are both refused,
    /// non-retryably, and only the committed generation is granted.
    #[test]
    fn a_stale_or_uncommitted_generation_may_not_take_a_writer_lease() {
        use super::super::runtime::RuntimeKind;
        use super::super::seat;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let session = "7b1a2c3d-9999-4000-8000-000000000488";
        let short = super::super::sessions::short_id(session);
        seat::register(
            &state,
            &short,
            session,
            "native",
            None,
            "anthropic",
            "orchestrator",
            false,
            1,
        )
        .expect("register");

        let fence = |generation: u64| SeatFence {
            short: &short,
            generation,
        };
        // The seat's own generation is granted.
        let held = acquire_writer(&state, 2, "seat", &tree, Some(fence(1)))
            .expect("the committed generation holds the seat");
        drop(held);

        let prepared = seat::prepare_onto(
            &state,
            &short,
            "claude",
            None,
            RuntimeKind::Harness,
            seat::Cause::Manual,
            2,
        )
        .expect("prepare");

        // The successor of a prepared-but-uncommitted rollover may not write.
        let refusal = acquire_writer(&state, 2, "successor", &tree, Some(fence(prepared)))
            .expect_err("an uncommitted successor may not take a writer lease");
        let WriterRefusal::StaleSeat { stale } = &refusal else {
            panic!("expected a stale-seat refusal, got {refusal:?}");
        };
        assert_eq!(stale.reason, seat::StaleReason::Uncommitted);
        // Non-retryable: the diagnostic says so rather than pointing at
        // contention or `--worktree`.
        let description = describe_writer_refusal(&refusal, &state, 2, &tree);
        assert!(
            description.contains("retrying will not change that"),
            "{description}"
        );
        // ...and the source still holds the seat while the transaction is open.
        let source = acquire_writer(&state, 2, "source", &tree, Some(fence(1)))
            .expect("the source keeps the seat until the commit");
        drop(source);

        seat::commit(&state, &short, prepared, "successor-session", 3).expect("commit");

        // After the commit the answer swaps, and the predecessor is refused
        // as superseded rather than as uncommitted.
        let refusal = acquire_writer(&state, 2, "source", &tree, Some(fence(1)))
            .expect_err("a superseded predecessor may not take a writer lease");
        let WriterRefusal::StaleSeat { stale } = &refusal else {
            panic!("expected a stale-seat refusal, got {refusal:?}");
        };
        assert_eq!(stale.reason, seat::StaleReason::Superseded);
        let successor = acquire_writer(&state, 2, "successor", &tree, Some(fence(prepared)))
            .expect("the committed successor holds the seat");
        drop(successor);

        // And a caller that presents no fence at all, with no seat env set,
        // is not fenced: a bare terminal and a CI job keep working.
        let elsewhere = tmp.path().join("other");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        acquire_writer(&state, 2, "unseated", &elsewhere, None)
            .expect("a caller outside any seat is never fenced");
    }

    // Issue #543 (review F4): the call-site wiring this test module used to
    // claim to cover -- `agent.rs`/`dash/mod.rs`/`native_worker.rs` each
    // building an explicit `SeatFence` instead of passing `None`
    // unconditionally -- was retired from here (the removed `a_call_site_
    // built_fence_refuses_an_uncommitted_generation_the_env_fence_let_
    // through`). Constructing a `SeatFence` directly and asserting on it, as
    // that test did, only re-proves `acquire_writer`'s own strict-fence
    // behavior, already covered above by
    // `a_stale_or_uncommitted_generation_may_not_take_a_writer_lease`; it
    // passed identically whether or not any call site was ever wired up.
    //
    // The real wiring is now exercised at the call site itself: `dash::
    // mod::tests::
    // fulfill_spawn_request_refuses_a_writer_lease_for_an_uncommitted_
    // requester_generation` drives `fulfill_spawn_request` end to end with
    // `SpawnRequest::parent_session`/`parent_seat_generation` naming an
    // uncommitted rollover, so it fails if that site ever goes back to
    // fencing on the dashboard's own (unrelated) environment instead of the
    // requester's. `agent.rs`'s call site is exercised the same indirect way
    // `run_with_refuses_a_second_writing_worker_into_a_tree_with_a_live_
    // writer` already covers writer-permit acquisition through `run_with`
    // (that test does not itself set an uncommitted generation, but goes
    // through the identical `acquire_writer` call this rewrite proves
    // refuses one). `native_worker.rs`'s call site has no dedicated
    // writer-permit test at all today -- a pre-existing gap this fix round
    // did not create and does not claim to close.

    /// The other half of the same rule: a DIFFERENT tree must never be
    /// refused just because some other tree already has a live writer.
    #[test]
    fn a_writer_in_a_different_tree_is_granted_even_while_another_tree_is_busy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held_a = acquire_writer(&state, 2, "worker-a", &tree_a, None).expect("granted");
        assert!(
            acquire_writer(&state, 2, "worker-b", &tree_b, None).is_ok(),
            "a different tree must not be refused by another tree's writer"
        );
    }

    /// Issue #338: zero removes only the machine-wide bound. Writers in
    /// different trees are both recorded, while the atomic tree claim still
    /// refuses a second writer in either occupied tree.
    #[test]
    fn zero_writer_limit_allows_different_trees_but_keeps_tree_exclusivity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held_a = acquire_writer(&state, 0, "worker-a", &tree_a, None).expect("first granted");
        let _held_b = acquire_writer(&state, 0, "worker-b", &tree_b, None).expect("second granted");
        let err = acquire_writer(&state, 0, "worker-c", &tree_a, None)
            .expect_err("the occupied tree must still be exclusive");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            }
        );

        let records = live_writer_records(&state);
        assert_eq!(records.len(), 2, "both live writers must remain visible");
        assert!(records.iter().any(|record| record.label == "worker-a"));
        assert!(records.iter().any(|record| record.label == "worker-b"));
    }

    /// Issue #338: zero is the opt-out, not a removal of the configurable
    /// machine-wide policy. An explicit bound of one preserves the prior
    /// cross-tree refusal.
    #[test]
    fn explicit_writer_limit_one_preserves_the_machine_wide_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held = acquire_writer(&state, 1, "worker-a", &tree_a, None).expect("first granted");
        let err = acquire_writer(&state, 1, "worker-b", &tree_b, None)
            .expect_err("the configured machine-wide bound must be enforced");
        assert_eq!(err, WriterRefusal::PoolExhausted);
    }

    /// Issue #338: an exhausted machine-wide pool names both operator
    /// controls and every live holder, including the repository tree that
    /// status alone previously made hard to correlate with the refusal.
    #[test]
    fn pool_exhaustion_description_names_the_controls_count_and_holders() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held = acquire_writer(&state, 1, "worker-a", &tree_a, None).expect("first granted");
        let description =
            describe_writer_refusal(&WriterRefusal::PoolExhausted, &state, 1, &tree_b);

        assert!(description.starts_with("writer-busy:"));
        assert!(description.contains("1 of 1 in use"));
        assert!(description.contains("supervise.max_writers"));
        assert!(description.contains("ZIRV_CTX_SUPERVISE_MAX_WRITERS"));
        assert!(description.contains("0 lifts the machine-wide cap"));
        assert!(description.contains(&format!(
            "pid {} -- worker-a -- {}",
            std::process::id(),
            tree_a.display()
        )));
    }

    /// The writer pool's own bound is independent of the heavy pool's --
    /// exhausting one must never affect the other, mirroring `a_permit_is_
    /// bounded_and_released_on_drop` for the heavy pool.
    #[test]
    fn the_writer_pool_is_bounded_independently_of_the_heavy_pool() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _held_a = acquire_writer(&state, 1, "worker-a", &tree_a, None).expect("granted");
        let err = acquire_writer(&state, 1, "worker-b", &tree_b, None)
            .expect_err("a writer pool of 1 is exhausted by the first writer");
        assert_eq!(err, WriterRefusal::PoolExhausted);

        assert_eq!(
            live_count(&state),
            0,
            "the heavy pool must be untouched by writer acquisitions"
        );
        assert!(
            acquire(&state, 1, "cargo build").is_some(),
            "the heavy pool is independent of the writer pool"
        );
    }

    /// A writer permit left by a dead process must not wedge its tree
    /// forever -- the writer-pool counterpart of `a_permit_left_by_a_dead_
    /// process_is_swept`.
    #[test]
    fn a_writer_permit_left_by_a_dead_process_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let dead_pid = crate::commands::ctx::testenv::dead_pid();

        let dir = writer_permits_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid: dead_pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "worker-a".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        state::write_private(&slot_path(&dir, 0), &json).expect("write");

        assert_eq!(
            live_writer_records(&state).len(),
            0,
            "a dead owner's writer permit does not count"
        );
        assert!(
            acquire_writer(&state, 1, "worker-b", &tree, None).is_ok(),
            "the tree is free again once the dead owner's permit is swept"
        );
    }

    /// Review finding (2026-09): with `max_writers = 2` (room in the pool for
    /// both), a second `acquire_writer` for the SAME tree must still be
    /// refused as `TreeBusy` -- before the fix, the tree-exclusivity check
    /// was a plain read-then-create with no atomicity of its own, so two
    /// concurrent callers could each see no live writer for the tree yet and
    /// both go on to claim a different (free) pool slot.
    #[test]
    fn two_writers_for_the_same_tree_never_both_succeed_even_with_a_free_slot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let _first =
            acquire_writer(&state, 2, "worker-a", &tree, None).expect("first writer granted");
        let err = acquire_writer(&state, 2, "worker-b", &tree, None)
            .expect_err("a second writer for the same tree must be refused");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            },
            "the free slot must not let a second writer share the tree"
        );
    }

    /// The atomic tree claim must not regress the existing "different trees
    /// never block each other" rule.
    #[test]
    fn different_trees_still_both_succeed_under_the_atomic_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree_a = tmp.path().join("repo-a");
        let tree_b = tmp.path().join("repo-b");
        std::fs::create_dir_all(&tree_a).expect("mkdir");
        std::fs::create_dir_all(&tree_b).expect("mkdir");

        let _a = acquire_writer(&state, 2, "worker-a", &tree_a, None).expect("granted");
        let _b = acquire_writer(&state, 2, "worker-b", &tree_b, None).expect("granted");
    }

    /// Dropping the first writer frees the tree claim for a third caller --
    /// the permit guard must release both the pool slot AND the tree claim
    /// together, or the tree would stay wedged after the slot alone freed.
    #[test]
    fn dropping_a_writer_frees_the_tree_for_a_new_caller() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let first =
            acquire_writer(&state, 2, "worker-a", &tree, None).expect("first writer granted");
        assert!(acquire_writer(&state, 2, "worker-b", &tree, None).is_err());

        drop(first);

        let third = acquire_writer(&state, 2, "worker-c", &tree, None)
            .expect("dropping the first writer must free the tree claim too, not just the slot");
        drop(third);
    }

    /// A stale tree claim left by a dead process must not wedge its tree
    /// forever -- mirrors `a_writer_permit_left_by_a_dead_process_is_swept`,
    /// but targets the NEW per-tree claim file directly rather than a slot
    /// file, proving `claim_tree`'s own dead-owner sweep (not just the pool
    /// slot's) reclaims it.
    #[test]
    fn a_stale_tree_claim_from_a_dead_pid_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let dead_pid = crate::commands::ctx::testenv::dead_pid();

        let key = tree_key(&tree);
        let dir = tree_claims_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let record = PermitRecord {
            pid: dead_pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "worker-a".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        create_new_private(&tree_claim_path(&dir, &key), &json).expect("write stale claim");

        assert!(
            acquire_writer(&state, 1, "worker-b", &tree, None).is_ok(),
            "a dead owner's tree claim must be swept, freeing the tree"
        );
    }

    #[test]
    fn concurrent_stale_writer_sweep_admits_exactly_one_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let key = tree_key(&tree);
        let dir = tree_claims_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let stale = PermitRecord {
            pid: crate::commands::ctx::testenv::dead_pid(),
            pid_start_time: None,
            child_start_time: None,
            child_pid: None,
            label: "dead".to_string(),
            acquired_at: 1,
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        create_new_private(
            &tree_claim_path(&dir, &key),
            &serde_json::to_string_pretty(&stale).expect("serialize"),
        )
        .expect("stale claim");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let spawn = |label: &'static str| {
            let state = state.clone();
            let tree = tree.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                acquire_writer(&state, 2, label, &tree, None)
            })
        };
        let first = spawn("first");
        let second = spawn("second");
        barrier.wait();
        let results = [first.join().expect("first"), second.join().expect("second")];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = results.into_iter().find_map(Result::ok).expect("winner");
        assert_eq!(live_writer_records(&state).len(), 1);
        let claim: PermitRecord = serde_json::from_str(
            &std::fs::read_to_string(tree_claim_path(&dir, &key)).expect("winner claim"),
        )
        .expect("parse winner claim");
        assert!(matches!(claim.label.as_str(), "first" | "second"));
        drop(winner);
    }

    #[test]
    fn distinct_tree_claims_share_one_bounded_lock_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        for index in 0..8 {
            let tree = tmp.path().join(format!("repo-{index}"));
            std::fs::create_dir_all(&tree).expect("mkdir");
            drop(acquire_writer(&state, 1, "worker", &tree, None).expect("writer"));
        }

        let lock_files = std::fs::read_dir(tree_claims_dir(&state))
            .expect("tree claims")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".lock"))
            .count();
        assert_eq!(lock_files, 1);
    }

    /// `live_writer_records` must keep returning exactly the pool slots it
    /// always did -- a tree claim (living under its own
    /// `writers/trees/` subdirectory) must never be double-counted as a
    /// second writer.
    #[test]
    fn live_writer_count_counts_slots_not_tree_claims() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let _held = acquire_writer(&state, 2, "worker-a", &tree, None).expect("granted");
        assert_eq!(
            live_writer_records(&state).len(),
            1,
            "one writer holds one slot -- the tree claim file must not also be counted"
        );
    }

    /// Review finding (2026-09): `set_child_pid` must propagate the SAME
    /// `child_pid` onto the paired tree claim, not just the pool slot --
    /// proven by acquiring a real writer permit, calling `set_child_pid`,
    /// and reading the tree claim file back off disk directly (`live_
    /// writer_records`/`set_child_pid_persists_to_the_permit_file` already
    /// cover the pool-slot half of this for the heavy pool).
    #[test]
    fn set_child_pid_also_persists_to_the_paired_tree_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let permit = acquire_writer(&state, 1, "worker-a", &tree, None).expect("granted");
        permit.set_child_pid(4242);

        let key = tree_key(&tree);
        let claim_path = tree_claim_path(&tree_claims_dir(&state), &key);
        let contents = std::fs::read_to_string(&claim_path).expect("read tree claim");
        let record: PermitRecord = serde_json::from_str(&contents).expect("parse");
        assert_eq!(
            record.child_pid,
            Some(4242),
            "the tree claim must carry the same child pid as the pool slot"
        );
    }

    /// Review finding (2026-09), acceptance: with the child pid propagated
    /// (as `set_child_pid` now does), a tree claim whose recorded PARENT pid
    /// is dead but whose CHILD pid is alive must not be swept -- mirrors
    /// `a_permit_stays_live_on_a_dead_parent_if_its_child_is_still_alive`'s
    /// own shape, but for the tree-claim file's own sweep in `claim_tree`
    /// rather than `live_records_in`'s. A second `acquire_writer` for the
    /// same tree, even with room in the pool (`max_writers = 2`), must still
    /// be refused as `TreeBusy`.
    #[test]
    fn a_tree_claim_stays_live_on_a_dead_parent_if_its_child_is_still_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");
        let dead_pid = crate::commands::ctx::testenv::dead_pid();

        let dir = tree_claims_dir(&state);
        state::create_private_dir_all(&dir).expect("mkdir");
        let key = tree_key(&tree);
        let record = PermitRecord {
            pid: dead_pid,
            pid_start_time: None,
            child_start_time: None,
            child_pid: Some(std::process::id()),
            label: "worker-a".to_string(),
            acquired_at: state::now_secs(),
            kind: PermitKind::Writer,
            tree: Some(tree.clone()),
        };
        let json = serde_json::to_string_pretty(&record).expect("serialize");
        create_new_private(&tree_claim_path(&dir, &key), &json).expect("write");

        let err = acquire_writer(&state, 2, "worker-b", &tree, None)
            .expect_err("a live child must keep the tree claim even though the parent pid is dead");
        assert_eq!(
            err,
            WriterRefusal::TreeBusy {
                holder_label: "worker-a".to_string()
            },
            "the tree claim must not be swept just because the recorded parent pid is dead"
        );
    }
}
