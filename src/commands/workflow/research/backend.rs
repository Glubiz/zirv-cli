//! The trial spec/result wire contract (`spec.json` -> backend ->
//! `trial.json`) and the two backend kinds: `command` (a real subprocess,
//! e.g. `run.py`) and `fixture` (in-process, deterministic, for tests and
//! the demo campaign). Both write `trial.json` in exactly the same shape so
//! everything downstream is backend-agnostic -- issue #802.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::manifest::{Backend, BackendKind};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::supervise::{Outcome, Tick, isolate_process_tree, supervise_child};

pub const TRIAL_SPEC_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct RouteSpec {
    pub harness: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrialSpec {
    pub schema: u32,
    pub campaign: String,
    pub candidate: String,
    pub arm: String,
    pub trial_id: String,
    pub task: String,
    pub rep: u32,
    pub split: String,
    pub stage: String,
    pub route: RouteSpec,
    pub env: BTreeMap<String, String>,
    pub state_dir: String,
    pub timeout_secs: u64,
    pub zirv_dir: Option<String>,
    pub strategy: Option<serde_json::Value>,
    pub cache_mode: String,
    pub pressure: String,
}

impl TrialSpec {
    pub fn write(&self, trial_dir: &Path) -> CtxResult<PathBuf> {
        std::fs::create_dir_all(trial_dir)
            .map_err(|err| format!("could not create '{}': {err}", trial_dir.display()))?;
        let path = trial_dir.join("spec.json");
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .map_err(|err| format!("could not write '{}': {err}", path.display()))?;
        Ok(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialStatus {
    Ok,
    Error,
    Timeout,
    Crash,
}

impl TrialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TrialStatus::Ok => "ok",
            TrialStatus::Error => "error",
            TrialStatus::Timeout => "timeout",
            TrialStatus::Crash => "crash",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Money {
    #[serde(default)]
    pub reported_usd: Option<f64>,
    #[serde(default)]
    pub estimated_usd: Option<f64>,
    #[serde(default)]
    pub unknown_count: u64,
    #[serde(default)]
    pub calls: u64,
}

impl Money {
    /// `reported + estimated`, `None` when both are unset.
    pub fn total(&self) -> Option<f64> {
        match (self.reported_usd, self.estimated_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpendReport {
    #[serde(default)]
    pub execution: Money,
    #[serde(default)]
    pub overhead: Money,
    #[serde(default)]
    pub calls: u64,
    #[serde(default)]
    pub completeness: String,
    #[serde(default)]
    pub receipts: BTreeMap<String, u64>,
}

impl SpendReport {
    pub fn cost_usd(&self) -> Option<f64> {
        self.execution.total()
    }

    pub fn cost_complete(&self) -> bool {
        self.completeness == "complete"
    }

    pub fn overhead_usd(&self) -> f64 {
        self.overhead.total().unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialResult {
    pub schema: u32,
    pub trial_id: String,
    pub status: TrialStatus,
    #[serde(default)]
    pub correctness: Option<f64>,
    #[serde(default)]
    pub quality: Option<f64>,
    pub wall_ms: u64,
    #[serde(default)]
    pub spend: Option<SpendReport>,
    #[serde(default)]
    pub route: Option<serde_json::Value>,
    #[serde(default)]
    pub env_fingerprint: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
}

impl TrialResult {
    pub fn path(trial_dir: &Path) -> PathBuf {
        trial_dir.join("trial.json")
    }

    pub fn write(&self, trial_dir: &Path) -> CtxResult<()> {
        let path = Self::path(trial_dir);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .map_err(|err| format!("could not write '{}': {err}", path.display()))?;
        Ok(())
    }

    /// Reads and parses `trial.json`; a missing or unparseable file is not
    /// an error here -- the caller treats `None` as a crash, per contract.
    pub fn read(trial_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(trial_dir)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn cost_usd(&self) -> Option<f64> {
        self.spend.as_ref().and_then(|s| s.cost_usd())
    }

    pub fn cost_complete(&self) -> bool {
        self.spend
            .as_ref()
            .map(|s| s.cost_complete())
            .unwrap_or(false)
    }

    pub fn overhead_usd(&self) -> f64 {
        self.spend.as_ref().map(|s| s.overhead_usd()).unwrap_or(0.0)
    }

    pub fn calls(&self) -> u64 {
        self.spend.as_ref().map(|s| s.calls).unwrap_or(0)
    }

    pub fn receipts(&self) -> BTreeMap<String, u64> {
        self.spend
            .as_ref()
            .map(|s| s.receipts.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrialOutcome {
    Finished(Box<TrialResult>),
    Crash { reason: String },
    Timeout,
}

impl PartialEq for TrialResult {
    fn eq(&self, other: &Self) -> bool {
        self.trial_id == other.trial_id && self.status == other.status
    }
}

/// A fixture backend's scripted rows -- `[[result]]` entries in
/// `backend.file`, matched by `(arm, task, rep)` with `task = "*"` and
/// `rep = None` as wildcards. The most specific match wins.
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureFile {
    #[serde(rename = "result", default)]
    pub results: Vec<FixtureRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FixtureRow {
    pub arm: String,
    #[serde(default = "default_wildcard_task")]
    pub task: String,
    #[serde(default)]
    pub rep: Option<u32>,
    #[serde(default = "default_ok_status")]
    pub status: String,
    #[serde(default)]
    pub correctness: Option<f64>,
    #[serde(default)]
    pub quality: Option<f64>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub wall_ms: Option<u64>,
    #[serde(default)]
    pub receipts: BTreeMap<String, u64>,
    #[serde(default)]
    pub delay_ms: Option<u64>,
    #[serde(default)]
    pub crash_first: u32,
}

fn default_wildcard_task() -> String {
    "*".to_string()
}

fn default_ok_status() -> String {
    "ok".to_string()
}

impl FixtureFile {
    pub fn load(path: &Path) -> CtxResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read fixture '{}': {err}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }

    /// Every row matching `(arm, task, rep)`, most specific first:
    /// `(arm, task, rep)` beats `(arm, task, *)` beats `(arm, *, *)`. More
    /// than one row can legitimately match -- a rep-specific row that only
    /// covers a `crash_first` window, plus a wildcard-rep row supplying the
    /// eventual real result once that window is exhausted (see
    /// `run_fixture_trial`).
    fn matching_rows(&self, arm: &str, task: &str, rep: u32) -> Vec<&FixtureRow> {
        let mut matches: Vec<(&FixtureRow, u8)> = self
            .results
            .iter()
            .filter(|row| row.arm == arm)
            .filter(|row| row.task == "*" || row.task == task)
            .filter(|row| row.rep.map(|r| r == rep).unwrap_or(true))
            .map(|row| {
                let specificity = (row.task != "*") as u8 * 2 + row.rep.is_some() as u8;
                (row, specificity)
            })
            .collect();
        matches.sort_by_key(|(_, specificity)| std::cmp::Reverse(*specificity));
        matches.into_iter().map(|(row, _)| row).collect()
    }
}

/// Counts how many times a fixture backend actually ran a trial (i.e.
/// dispatched external "work") -- `plan` must never advance this.
pub static FIXTURE_DISPATCH_COUNT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub fn reset_fixture_dispatch_count() {
    FIXTURE_DISPATCH_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub fn fixture_dispatch_count() -> u64 {
    FIXTURE_DISPATCH_COUNT.load(Ordering::SeqCst)
}

/// How many fixture trials are inside their (simulated) execution window
/// right now, and the high-water mark since the last reset -- lets a test
/// assert the scheduler's `concurrency` cap directly instead of only
/// inferring it from timing.
pub static FIXTURE_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
pub static FIXTURE_MAX_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub fn reset_fixture_concurrency_tracking() {
    FIXTURE_IN_FLIGHT.store(0, Ordering::SeqCst);
    FIXTURE_MAX_IN_FLIGHT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub fn fixture_max_in_flight() -> u64 {
    FIXTURE_MAX_IN_FLIGHT.load(Ordering::SeqCst)
}

fn env_fingerprint(env: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (k, v) in env {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Runs one trial through `backend`, writing `trial.json` for the fixture
/// kind (a command backend is expected to write it itself) and returning
/// the outcome the scheduler acts on.
pub fn run_trial(
    backend: &Backend,
    spec: &TrialSpec,
    trial_dir: &Path,
    repo: &Path,
    fixture_dir: &Path,
    attempt: u32,
) -> CtxResult<TrialOutcome> {
    spec.write(trial_dir)?;
    match backend.kind {
        BackendKind::Fixture => run_fixture_trial(backend, spec, trial_dir, fixture_dir, attempt),
        BackendKind::Command => run_command_trial(backend, spec, trial_dir, repo),
    }
}

fn run_fixture_trial(
    backend: &Backend,
    spec: &TrialSpec,
    trial_dir: &Path,
    fixture_dir: &Path,
    attempt: u32,
) -> CtxResult<TrialOutcome> {
    FIXTURE_DISPATCH_COUNT.fetch_add(1, Ordering::SeqCst);
    let file = backend
        .file
        .as_ref()
        .ok_or("backend.file is required for a fixture backend")?;
    let fixture = FixtureFile::load(&fixture_dir.join(file))?;
    // The fixture format's own `arm` field names WHICH side of the pair a
    // row is for -- `"baseline"` or a `[[candidates]] id` -- matching
    // `spec.candidate`, not `spec.arm` (which only ever holds the generic
    // `"baseline"|"candidate"` the trial-spec JSON contract documents).
    let candidates = fixture.matching_rows(&spec.candidate, &spec.task, spec.rep);

    if candidates.is_empty() {
        return Ok(TrialOutcome::Crash {
            reason: format!(
                "no fixture result for candidate='{}' task='{}' rep={}",
                spec.candidate, spec.task, spec.rep
            ),
        });
    }

    // Walk the matches most-specific-first: a row still inside its own
    // `crash_first` window governs this attempt outright (crash). A row
    // past its window but whose own `status` is itself `"crash"` (a
    // crash-only placeholder with no result data of its own, e.g. one that
    // exists only to script "rep N crashes on attempt 0") is spent and
    // falls through to the next best match -- typically a wildcard-rep row
    // carrying the real eventual result. Reaching the end of the list with
    // nothing chosen means every match was a crash placeholder, past or
    // present -- also a crash.
    let mut chosen: Option<&FixtureRow> = None;
    for row in &candidates {
        if let Some(delay_ms) = row.delay_ms {
            let current = FIXTURE_IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            FIXTURE_MAX_IN_FLIGHT.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(delay_ms));
            FIXTURE_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
        if attempt < row.crash_first {
            return Ok(TrialOutcome::Crash {
                reason: "fixture-scripted crash".to_string(),
            });
        }
        if row.status != "crash" {
            chosen = Some(row);
            break;
        }
    }

    let Some(row) = chosen else {
        return Ok(TrialOutcome::Crash {
            reason: "fixture-scripted crash".to_string(),
        });
    };

    let status = match row.status.as_str() {
        "ok" => TrialStatus::Ok,
        "error" => TrialStatus::Error,
        "timeout" => TrialStatus::Timeout,
        other => return Err(format!("fixture row has an unknown status '{other}'").into()),
    };

    let spend = SpendReport {
        execution: Money {
            reported_usd: row.cost_usd,
            estimated_usd: None,
            unknown_count: if row.cost_usd.is_none() { 1 } else { 0 },
            calls: 1,
        },
        overhead: Money::default(),
        calls: 1,
        completeness: if row.cost_usd.is_some() {
            "complete".to_string()
        } else {
            "unknown".to_string()
        },
        receipts: row.receipts.clone(),
    };

    let result = TrialResult {
        schema: TRIAL_SPEC_SCHEMA,
        trial_id: spec.trial_id.clone(),
        status,
        correctness: row.correctness,
        quality: row.quality,
        wall_ms: row.wall_ms.unwrap_or(0),
        spend: Some(spend),
        route: Some(serde_json::json!({
            "harness": spec.route.harness,
            "model": spec.route.model,
        })),
        env_fingerprint: Some(env_fingerprint(&spec.env)),
        details: None,
    };
    result.write(trial_dir)?;
    Ok(TrialOutcome::Finished(Box::new(result)))
}

fn substitute(template: &str, spec_path: &Path, out_dir: &Path, zirv_dir: Option<&str>) -> String {
    template
        .replace("{spec}", &spec_path.to_string_lossy())
        .replace("{out}", &out_dir.to_string_lossy())
        .replace("{zirv_dir}", zirv_dir.unwrap_or(""))
}

fn run_command_trial(
    backend: &Backend,
    spec: &TrialSpec,
    trial_dir: &Path,
    repo: &Path,
) -> CtxResult<TrialOutcome> {
    let command_template = backend
        .command
        .as_ref()
        .ok_or("backend.command is required for a command backend")?;
    if command_template.is_empty() {
        return Err("backend.command is empty".into());
    }
    let spec_path = trial_dir.join("spec.json");
    let argv: Vec<String> = command_template
        .iter()
        .map(|token| substitute(token, &spec_path, trial_dir, spec.zirv_dir.as_deref()))
        .collect();

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.current_dir(repo);
    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command.env("ZIRV_CTX_STATE_DIR", &spec.state_dir);
    command.env("ZIRV_ATTR_CAMPAIGN", &spec.campaign);
    command.env("ZIRV_ATTR_CANDIDATE", &spec.candidate);
    command.env("ZIRV_ATTR_TRIAL", &spec.trial_id);
    command.env("ZIRV_ATTR_TASK", &spec.task);
    isolate_process_tree(&mut command);

    let mut child = command
        .spawn()
        .map_err(|err| format!("could not spawn backend command {argv:?}: {err}"))?;
    let deadline = Instant::now() + Duration::from_secs(spec.timeout_secs.max(1));
    let outcome = supervise_child(
        &mut child,
        deadline,
        Duration::from_millis(250),
        &mut || Tick::Continue,
    )?;

    match outcome {
        Outcome::TimedOut => Ok(TrialOutcome::Timeout),
        Outcome::StoppedByTick(_) => unreachable!("on_tick never asks to stop"),
        Outcome::Exited(code) => match TrialResult::read(trial_dir) {
            Some(result) => Ok(TrialOutcome::Finished(Box::new(result))),
            None => Ok(TrialOutcome::Crash {
                reason: format!("backend exited {code} without a valid trial.json"),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(arm: &str, task: &str, rep: u32) -> TrialSpec {
        TrialSpec {
            schema: TRIAL_SPEC_SCHEMA,
            campaign: "demo".into(),
            candidate: if arm == "baseline" {
                "baseline".into()
            } else {
                "cand-a".into()
            },
            arm: arm.into(),
            trial_id: format!("{arm}-{task}-{rep}"),
            task: task.into(),
            rep,
            split: "dev".into(),
            stage: "screen".into(),
            route: RouteSpec {
                harness: "claude".into(),
                model: "sonnet".into(),
            },
            env: BTreeMap::new(),
            state_dir: "state".into(),
            timeout_secs: 60,
            zirv_dir: None,
            strategy: None,
            cache_mode: "cold".into(),
            pressure: "natural".into(),
        }
    }

    fn fixture_backend(file: &str) -> Backend {
        Backend {
            kind: BackendKind::Fixture,
            command: None,
            file: Some(PathBuf::from(file)),
            per_trial_ceiling_usd: 1.0,
            calls_per_trial: 4,
            timeout_secs: 60,
        }
    }

    #[test]
    fn a_fixture_backend_writes_trial_json_matching_the_scripted_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fixture.toml"),
            r#"
[[result]]
arm = "baseline"
task = "t1"
status = "ok"
correctness = 1.0
quality = 0.9
cost_usd = 0.05
wall_ms = 1200
"#,
        )
        .unwrap();
        let trial_dir = dir.path().join("trial");
        let outcome = run_trial(
            &fixture_backend("fixture.toml"),
            &spec("baseline", "t1", 0),
            &trial_dir,
            dir.path(),
            dir.path(),
            0,
        )
        .unwrap();
        let TrialOutcome::Finished(result) = outcome else {
            panic!("expected a finished outcome");
        };
        assert_eq!(result.status, TrialStatus::Ok);
        assert_eq!(result.correctness, Some(1.0));
        assert!(TrialResult::path(&trial_dir).is_file());
    }

    #[test]
    fn crash_first_crashes_the_first_n_attempts_then_recovers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fixture.toml"),
            r#"
[[result]]
arm = "baseline"
task = "*"
status = "ok"
correctness = 1.0
cost_usd = 0.01
crash_first = 2
"#,
        )
        .unwrap();
        for attempt in 0..2 {
            let trial_dir = dir.path().join(format!("t{attempt}"));
            let outcome = run_trial(
                &fixture_backend("fixture.toml"),
                &spec("baseline", "t1", 0),
                &trial_dir,
                dir.path(),
                dir.path(),
                attempt,
            )
            .unwrap();
            assert!(matches!(outcome, TrialOutcome::Crash { .. }));
        }
        let trial_dir = dir.path().join("t2");
        let outcome = run_trial(
            &fixture_backend("fixture.toml"),
            &spec("baseline", "t1", 0),
            &trial_dir,
            dir.path(),
            dir.path(),
            2,
        )
        .unwrap();
        assert!(matches!(outcome, TrialOutcome::Finished(_)));
    }

    #[test]
    fn a_missing_fixture_row_is_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fixture.toml"), "").unwrap();
        let trial_dir = dir.path().join("trial");
        let outcome = run_trial(
            &fixture_backend("fixture.toml"),
            &spec("baseline", "t1", 0),
            &trial_dir,
            dir.path(),
            dir.path(),
            0,
        )
        .unwrap();
        assert!(matches!(outcome, TrialOutcome::Crash { .. }));
        assert!(!TrialResult::path(&trial_dir).is_file());
    }

    #[test]
    fn a_real_command_that_outlives_its_timeout_is_reported_as_timeout_and_the_child_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let command = if cfg!(windows) {
            vec![
                "powershell".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Start-Sleep 30".to_string(),
            ]
        } else {
            vec!["sleep".to_string(), "30".to_string()]
        };
        let backend = Backend {
            kind: BackendKind::Command,
            command: Some(command),
            file: None,
            per_trial_ceiling_usd: 1.0,
            calls_per_trial: 1,
            timeout_secs: 2,
        };
        let mut trial_spec = spec("baseline", "t1", 0);
        trial_spec.timeout_secs = 2;
        let trial_dir = dir.path().join("trial");
        std::fs::create_dir_all(&trial_dir).unwrap();
        let outcome =
            run_trial(&backend, &trial_spec, &trial_dir, dir.path(), dir.path(), 0).unwrap();
        assert_eq!(outcome, TrialOutcome::Timeout);
    }
}
