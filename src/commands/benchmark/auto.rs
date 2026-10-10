//! `zirv benchmark auto`: unattended synthetic probes that feed the routing evidence store.
//!
//! The run is gated by a daily interval, a per-harness headroom floor and a spend cap, and
//! never prompts. Models probed are new or stale ones (see [`select_targets`]); the rows land
//! in the ordinary benchmark store, and `evidence` and `promotion` are refreshed afterwards.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use serde::{Deserialize, Serialize};

use super::run::{Row, Status};
use super::{Filters, Launcher, run};
use crate::commands::ctx::adapters::{self, Liveness};
use crate::commands::ctx::catalogue;
use crate::commands::ctx::config::{CtxConfig, EnvLookup, RoutingConfig};
use crate::commands::ctx::models;
use crate::commands::ctx::models::evidence::{self, Evidence, MIN_SYNTH};
use crate::commands::ctx::models::promotion::{self, CandidateStatus, Promotions};
use crate::commands::ctx::pace;
use crate::commands::ctx::price::format_usd;
use crate::commands::ctx::state::{self, StateDir};

const LOCK_FILE: &str = "probe.lock";
const LAST_RUN_FILE: &str = "probe-last-run";
const ATTEMPT_FILE: &str = "probe-attempt";
const LOG_FILE: &str = "probe.jsonl";
/// A failed or skipped start is not retried sooner than this.
const ATTEMPT_BACKOFF_SECS: u64 = 3600;
/// A model whose synthetic evidence is older than this is probed again.
const STALE_SECS: u64 = 7 * 24 * 3600;
const MAX_PER_HARNESS: usize = 4;
/// How long the detached probe waits for `models refresh` to release its lock before skipping
/// the post-run bookkeeping.
const BOOKKEEPING_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) struct Opts {
    pub force: bool,
    pub dry_run: bool,
    pub json: bool,
    pub quiet: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Target {
    harness: String,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Skip {
    harness: String,
    reason: String,
}

/// One line of `<state>/logs/probe.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeRecord {
    ts: u64,
    run_id: Option<String>,
    targets: Vec<Target>,
    skipped: Vec<Skip>,
    spend_micros: u64,
    outcome: String,
}

fn read_secs(state: &StateDir, name: &str) -> u64 {
    std::fs::read_to_string(state.root().join(name))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// The probe's own gate: probing is on and the (24 h floored) interval since the last run has
/// passed. The spawned child checks only this, since the spawner stamps the attempt first.
fn interval_due(routing: &RoutingConfig, state: &StateDir, now: u64) -> bool {
    routing.enabled
        && routing.probe
        && now.saturating_sub(read_secs(state, LAST_RUN_FILE)) >= routing.probe_interval_secs()
}

/// Whether the spawner should start a probe now: [`interval_due`] and no attempt in the last
/// hour. File reads only.
pub(crate) fn probe_due(routing: &RoutingConfig, state: &StateDir, now: u64) -> bool {
    interval_due(routing, state, now)
        && now.saturating_sub(read_secs(state, ATTEMPT_FILE)) >= ATTEMPT_BACKOFF_SECS
}

/// Starts one background probe when [`probe_due`]. Spawn-and-forget: `spawn` must not wait.
/// The attempt is recorded before the spawn, so a probe that fails to start backs off too.
pub(crate) fn spawn_probe_if_due(
    routing: &RoutingConfig,
    state: &StateDir,
    now: u64,
    spawn: &mut dyn FnMut() -> bool,
) -> bool {
    if !probe_due(routing, state, now) {
        return false;
    }
    if std::fs::create_dir_all(state.root()).is_err() {
        return false;
    }
    if state::write_private(&state.root().join(ATTEMPT_FILE), &now.to_string()).is_err() {
        return false;
    }
    spawn()
}

#[cfg(not(test))]
fn spawn_detached(routing: &RoutingConfig, state: &StateDir) {
    spawn_probe_if_due(routing, state, state::now_secs(), &mut || {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let mut command = std::process::Command::new(exe);
        command
            .args(["benchmark", "auto", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::commands::workflow::engine::cli::detach(&mut command);
        command.spawn().is_ok()
    });
}

/// The detached production trigger beside `spawn_refresh_if_due_detached`; never waits and a
/// no-op in test builds.
pub(crate) fn spawn_probe_if_due_detached(cfg: &CtxConfig, state: &StateDir) {
    #[cfg(not(test))]
    spawn_detached(&cfg.routing, state);
    #[cfg(test)]
    let _ = (cfg, state);
}

/// The same trigger for a caller that has not loaded the config: only the operator-level
/// `[routing]` table (repos cannot set it) and the state directory are read.
pub(crate) fn spawn_probe_if_due_from_env() {
    #[cfg(not(test))]
    {
        let env = crate::commands::ctx::config::env_from_process();
        if let (Ok(routing), Ok(state)) = (
            RoutingConfig::load_operator_only(&env),
            StateDir::resolve(&env),
        ) {
            spawn_detached(&routing, &state);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Headroom {
    Known(f64),
    Unknown { pacing_on: bool },
}

/// What target selection needs to know about one installed, enabled harness.
#[derive(Debug, Clone)]
struct HarnessInput {
    harness: String,
    metered: bool,
    headroom: Headroom,
    /// New-family candidates on probation (not unavailable), newest first.
    new_family: Vec<String>,
    /// Probation candidates, newest first.
    probation: Vec<String>,
    rejected: Vec<String>,
    /// Incumbents, the resolved ladder, then eligible new-family ids.
    ladder: Vec<String>,
}

/// Which models to probe, in priority order and capped per harness; and why a harness is not
/// probed. Pure.
fn select_targets(
    routing: &RoutingConfig,
    inputs: &[HarnessInput],
    evidence: &Evidence,
    now: u64,
) -> (Vec<Target>, Vec<Skip>) {
    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    for input in inputs {
        let skip = |reason: String| Skip {
            harness: input.harness.clone(),
            reason,
        };
        if input.metered && !routing.probe_metered {
            skipped.push(skip(
                "metered endpoint override ([routing] probe_metered = false)".to_string(),
            ));
            continue;
        }
        match input.headroom {
            Headroom::Known(pct) if pct <= 0.0 => {
                skipped.push(skip("usage limit reached".to_string()));
                continue;
            }
            Headroom::Known(pct) if pct < routing.probe_min_headroom_pct => {
                skipped.push(skip(format!(
                    "headroom {pct:.0}% is below the {:.0}% floor",
                    routing.probe_min_headroom_pct
                )));
                continue;
            }
            Headroom::Unknown { pacing_on: false } => {
                skipped.push(skip("headroom unknown and pacing is disabled".to_string()));
                continue;
            }
            Headroom::Known(_) | Headroom::Unknown { pacing_on: true } => {}
        }
        let synthetic = |model: &str| evidence.pooled(&input.harness, model).map(|c| c.synthetic);
        let probed_recently = |model: &str| {
            synthetic(model)
                .and_then(|s| s.last_at)
                .is_some_and(|at| now.saturating_sub(at) <= STALE_SECS)
        };
        let thin = |model: &str| synthetic(model).is_none_or(|s| s.n < MIN_SYNTH);
        let ordered = input
            .new_family
            .iter()
            .chain(input.probation.iter())
            .chain(input.rejected.iter().filter(|m| !probed_recently(m)))
            .chain(
                input
                    .ladder
                    .iter()
                    .filter(|m| !probed_recently(m) || thin(m)),
            );
        let mut chosen: Vec<&String> = Vec::new();
        for model in ordered {
            if chosen.len() == MAX_PER_HARNESS {
                break;
            }
            if !chosen.contains(&model) {
                chosen.push(model);
            }
        }
        targets.extend(chosen.into_iter().map(|model| Target {
            harness: input.harness.clone(),
            model: model.clone(),
        }));
    }
    (targets, skipped)
}

fn headroom_of(cfg: &CtxConfig, state: &StateDir, now: u64, harness: &str) -> Headroom {
    let provider = adapters::provider_for_agent_name(Some(harness));
    let (collector, estimator) = pace::current_windows(state, &cfg.pace, now, provider);
    match pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace) {
        Some(reading) => Headroom::Known(reading.headroom_pct),
        None => Headroom::Unknown {
            pacing_on: cfg.pace.enabled,
        },
    }
}

fn gather(
    cfg: &CtxConfig,
    state: &StateDir,
    now: u64,
    present: &dyn Fn(&str, &str) -> Liveness,
    promotions: &Promotions,
) -> Vec<HarnessInput> {
    let mut out = Vec::new();
    for (name, _) in adapters::ADAPTERS {
        if !cfg.agents.is_enabled(name) {
            continue;
        }
        let Ok((adapter, Liveness::Live | Liveness::Unknown(_))) =
            adapters::adapter_liveness_with(cfg, name, None, present)
        else {
            continue;
        };
        let vendor = adapter.provider();
        let prefix = format!("{vendor}.");
        let families = || {
            promotions
                .families
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(_, family)| family)
        };
        let rejected = families()
            .flat_map(|family| &family.candidates)
            .filter(|(_, candidate)| candidate.status == CandidateStatus::Rejected)
            .map(|(id, _)| id.clone())
            .collect();
        let rungs = catalogue::vendor(vendor)
            .map(|vendor| models::ladder_for(cfg, vendor))
            .unwrap_or_default();
        // Eligible new-family ids ride the ladder tier: their synthetic cells age out of the
        // evidence window like any other, and the router drops a model that falls below
        // MIN_SYNTH.
        let ladder = families()
            .map(|family| family.incumbent.clone())
            .chain(
                rungs
                    .iter()
                    .map(|rung| catalogue::normalize_id(&rung.id).to_lowercase()),
            )
            .chain(promotion::new_family_eligible(promotions, vendor))
            .collect();
        out.push(HarnessInput {
            harness: (*name).to_string(),
            metered: adapter.endpoint_vendor().is_some(),
            headroom: headroom_of(cfg, state, now, name),
            new_family: promotion::new_family_probation(promotions, vendor)
                .into_iter()
                .chain(promotion::unverified_candidates(promotions, vendor, now))
                .collect(),
            probation: promotion::probation_candidates(promotions, vendor),
            rejected,
            ladder,
        });
    }
    out
}

fn append_log(state: &StateDir, record: &ProbeRecord) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    let _ = std::fs::create_dir_all(state.logs());
    if let Ok(mut file) = state::open_private_append(&state.logs().join(LOG_FILE)) {
        let _ = writeln!(file, "{line}");
    }
}

/// `zirv benchmark auto`. Exits 0 when the lock is busy, the probe is not due, or there is
/// nothing to probe; only a planning or store failure is an error.
pub(crate) fn run(
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    opts: &Opts,
    python_present: bool,
    present: &dyn Fn(&str, &str) -> Liveness,
    injected: Option<&mut Launcher<'_>>,
) -> Result<i32, String> {
    let state = StateDir::resolve(env).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(state.root())
        .map_err(|e| format!("{}: {e}", state.root().display()))?;
    let now = state::now_secs();
    // A dry run reads and prints only, so it neither needs the lock nor honours the interval.
    let _lock = if opts.dry_run {
        None
    } else {
        match state::try_acquire_lock(&state.root().join(LOCK_FILE)) {
            Ok(lock) => Some(lock),
            Err(_) => return Ok(0),
        }
    };
    if !opts.dry_run && !opts.force && !interval_due(&cfg.routing, &state, now) {
        return Ok(0);
    }

    let evidence = evidence::load(&state).unwrap_or_default();
    let promotions = promotion::load(&state).unwrap_or_default();
    let inputs = gather(cfg, &state, now, present, &promotions);
    let (targets, skipped) = select_targets(&cfg.routing, &inputs, &evidence, now);

    let record = |outcome: &str, run_id: Option<String>, spend_micros: u64| ProbeRecord {
        ts: now,
        run_id,
        targets: targets.clone(),
        skipped: skipped.clone(),
        spend_micros,
        outcome: outcome.to_string(),
    };

    let mut plan = None;
    if !targets.is_empty() {
        let filters = Filters {
            models: targets
                .iter()
                .map(|t| (t.harness.clone(), t.model.clone()))
                .collect(),
            no_judge: true,
            ..Filters::default()
        };
        let listing = models::listing(cfg, &state);
        let planned = run::plan(
            cfg,
            &filters,
            1,
            cfg.routing.probe_max_usd,
            python_present,
            present,
            &listing,
        )
        .inspect_err(|error| {
            if !opts.dry_run {
                append_log(&state, &record(&format!("plan failed: {error}"), None, 0));
            }
        })?;
        plan = Some(planned).filter(|planned| planned.agent_runs > 0);
    }
    let Some(plan) = plan else {
        let outcome = if targets.is_empty() {
            "no targets"
        } else {
            "no runnable tasks"
        };
        if !opts.dry_run {
            append_log(&state, &record(outcome, None, 0));
        }
        return emit(opts, &record(outcome, None, 0), 0);
    };
    if opts.dry_run {
        return emit(opts, &record("dry run", None, 0), plan.agent_runs);
    }

    // Stamped before the first launch, so a crash mid-run still backs the next probe off.
    state::write_private(&state.root().join(LAST_RUN_FILE), &now.to_string())
        .map_err(|e| format!("{LAST_RUN_FILE}: {e}"))?;
    let ran = super::run_benchmark_with(
        cfg,
        env,
        &plan,
        super::DEFAULT_TIMEOUT_SECS,
        opts.quiet,
        injected,
    );
    let (run_id, spend, mut outcome) = match ran {
        Ok((report, spend)) => (Some(report.meta.run_id), spend, "ran".to_string()),
        Err(error) => (None, 0, format!("run failed: {error}")),
    };
    let refreshed = state::now_secs();
    // Under the refresher's lock: these read-modify-write the files `models refresh` writes.
    let bookkeeping = models::with_refresh_lock(&state, BOOKKEEPING_LOCK_WAIT, || {
        if let Some(run_id) = &run_id {
            record_availability(&state, env, &promotions, run_id, refreshed);
        }
        if let Err(error) = evidence::refresh(&state, cfg, refreshed) {
            Some(format!("evidence refresh failed: {error}"))
        } else if let Err(error) = promotion::refresh(&state, cfg, refreshed) {
            Some(format!("promotion refresh failed: {error}"))
        } else {
            None
        }
    });
    match bookkeeping {
        Some(Some(failure)) => outcome = format!("{outcome}; {failure}"),
        Some(None) => {}
        None => {
            outcome = format!(
                "{outcome}; availability and evidence not recorded: models refresh held its lock"
            );
        }
    }
    let done = record(&outcome, run_id, spend);
    append_log(&state, &done);
    emit(opts, &done, plan.agent_runs)
}

/// For each of `candidates` (normalized ids) that has rows in `rows`: whether any row ran, by
/// the same definition evidence counts samples with ([`evidence::row_ran`]). A candidate whose
/// rows all failed without output or a hang never ran on this account. Skipped rows say
/// nothing. Sorted by id.
fn probe_outcomes(rows: &[Row], candidates: &BTreeSet<String>) -> Vec<(String, bool)> {
    let mut seen: BTreeMap<String, bool> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.status != Status::Skipped) {
        let id = catalogue::normalize_id(row.candidate_label()).to_lowercase();
        if candidates.contains(&id) {
            *seen.entry(id).or_insert(false) |= evidence::row_ran(row);
        }
    }
    seen.into_iter().collect()
}

/// After a probe run: back off a new-family candidate that never ran, and record one that did
/// as available on this account. Best effort, like the rest of the post-run bookkeeping.
fn record_availability(
    state: &StateDir,
    env: EnvLookup<'_>,
    promotions: &Promotions,
    run_id: &str,
    now: u64,
) {
    let vendor_of = |id: &String| {
        promotions
            .new_families
            .get(id)
            .map(|c| c.vendor.clone())
            .or_else(|| promotions.unverified.get(id).map(|u| u.vendor.clone()))
    };
    let candidates: BTreeSet<String> = promotions
        .new_families
        .keys()
        .chain(promotions.unverified.keys())
        .cloned()
        .collect();
    if candidates.is_empty() {
        return;
    }
    let Ok(root) = super::benchmark_root(env) else {
        return;
    };
    let Ok((_, rows)) = super::run::load_store(&root.join(run_id)) else {
        return;
    };
    let outcomes = probe_outcomes(&rows, &candidates);
    let _ = promotion::save_probe_outcomes(state, &outcomes, now);
    let ran: Vec<(String, String)> = outcomes
        .iter()
        .filter(|(_, ran)| *ran)
        .filter_map(|(id, _)| Some((vendor_of(id)?, id.clone())))
        .collect();
    if !ran.is_empty() {
        let _ = models::mark_probe_available(state, &ran, now);
    }
}

fn emit(opts: &Opts, record: &ProbeRecord, agent_runs: usize) -> Result<i32, String> {
    if opts.json {
        let mut value = serde_json::to_value(record).map_err(|e| e.to_string())?;
        value["dry_run"] = opts.dry_run.into();
        value["agent_runs"] = agent_runs.into();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
    } else if !opts.quiet {
        println!("probe: {} ({} agent run(s))", record.outcome, agent_runs);
        for target in &record.targets {
            println!("  {}/{}", target.harness, target.model);
        }
        for skip in &record.skipped {
            println!("  skipped {}: {}", skip.harness, skip.reason);
        }
        if !opts.dry_run && record.run_id.is_some() {
            println!("  spend: {}", format_usd(record.spend_micros, false));
        }
    }
    Ok(0)
}

fn last_records(state: &StateDir) -> Vec<ProbeRecord> {
    std::fs::read_to_string(state.logs().join(LOG_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn when(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map_or_else(
            || "?".to_string(),
            |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
        )
}

/// The PROBES section of `zirv ctx models`: last run, next due, the last skip reasons and the
/// last run's spend.
pub(crate) fn render_probe_status(state: &StateDir, cfg: &CtxConfig, now: u64) -> String {
    let routing = &cfg.routing;
    if !routing.enabled {
        return "\nPROBES\tdisabled ([routing] enabled = false)\n".to_string();
    }
    if !routing.probe {
        return "\nPROBES\tdisabled ([routing] probe = false)\n".to_string();
    }
    let last_run = read_secs(state, LAST_RUN_FILE);
    let attempt = read_secs(state, ATTEMPT_FILE);
    let due_at = (last_run + routing.probe_interval_secs()).max(attempt + ATTEMPT_BACKOFF_SECS);
    let records = last_records(state);
    let mut out = String::from("\nPROBES\n");
    out.push_str(&format!(
        "  last run:  {}\n",
        if last_run == 0 {
            "never".to_string()
        } else {
            when(last_run)
        }
    ));
    out.push_str(&format!(
        "  next due:  {}\n",
        if due_at <= now {
            "on the next zirv command".to_string()
        } else {
            when(due_at)
        }
    ));
    match records.last() {
        Some(last) if !last.skipped.is_empty() => {
            for skip in &last.skipped {
                out.push_str(&format!("  skipped:   {}: {}\n", skip.harness, skip.reason));
            }
        }
        _ => out.push_str("  skipped:   none\n"),
    }
    match records.iter().rev().find(|record| record.run_id.is_some()) {
        Some(ran) => out.push_str(&format!(
            "  spend:     {} ({} model(s), {})\n",
            format_usd(ran.spend_micros, false),
            ran.targets.len(),
            ran.outcome
        )),
        None => out.push_str("  spend:     -\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::models::evidence::{
        Cell, CellComplexity, RealStats, RouteRole, SyntheticStats,
    };
    use crate::commands::ctx::models::promotion::{CandidateState, FamilyState};
    use crate::commands::ctx::testenv;
    use std::cell::Cell as Counter;
    use std::collections::HashMap;
    use std::path::Path;

    const NOW: u64 = 2_000_000_000;
    const DAY: u64 = 24 * 3600;

    fn state_in(dir: &Path) -> StateDir {
        StateDir::from_path(dir.to_path_buf())
    }

    fn stamp(state: &StateDir, name: &str, at: u64) {
        std::fs::write(state.root().join(name), at.to_string()).unwrap();
    }

    fn routing(hours: u64) -> RoutingConfig {
        RoutingConfig {
            probe_interval_hours: hours,
            ..RoutingConfig::default()
        }
    }

    #[test]
    fn probe_is_due_at_24h_but_not_a_minute_before() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        stamp(&state, LAST_RUN_FILE, NOW - DAY + 60);
        assert!(!probe_due(&routing(24), &state, NOW));
        stamp(&state, LAST_RUN_FILE, NOW - DAY);
        assert!(probe_due(&routing(24), &state, NOW));
    }

    #[test]
    fn an_interval_under_24h_is_clamped_to_24h() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        stamp(&state, LAST_RUN_FILE, NOW - 2 * 3600);
        assert!(!probe_due(&routing(1), &state, NOW));
        stamp(&state, LAST_RUN_FILE, NOW - DAY + 60);
        assert!(!probe_due(&routing(1), &state, NOW));
        stamp(&state, LAST_RUN_FILE, NOW - DAY);
        assert!(probe_due(&routing(1), &state, NOW));
    }

    #[test]
    fn a_recent_attempt_backs_the_probe_off_for_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        stamp(&state, ATTEMPT_FILE, NOW - 3599);
        assert!(!probe_due(&routing(24), &state, NOW));
        stamp(&state, ATTEMPT_FILE, NOW - 3600);
        assert!(probe_due(&routing(24), &state, NOW));
    }

    #[test]
    fn a_disabled_probe_is_never_due() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let off = RoutingConfig {
            probe: false,
            ..RoutingConfig::default()
        };
        assert!(!probe_due(&off, &state, NOW));
        let disabled = RoutingConfig {
            enabled: false,
            ..RoutingConfig::default()
        };
        assert!(!probe_due(&disabled, &state, NOW));
    }

    #[test]
    fn the_spawn_runs_only_when_due_and_records_the_attempt_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let calls = Counter::new(0);
        let attempt_seen = Counter::new(false);
        let mut spawn = || {
            calls.set(calls.get() + 1);
            attempt_seen.set(state.root().join(ATTEMPT_FILE).exists());
            true
        };
        stamp(&state, LAST_RUN_FILE, NOW - 60);
        assert!(!spawn_probe_if_due(&routing(24), &state, NOW, &mut spawn));
        assert_eq!(calls.get(), 0);
        stamp(&state, LAST_RUN_FILE, NOW - DAY);
        assert!(spawn_probe_if_due(&routing(24), &state, NOW, &mut spawn));
        assert_eq!((calls.get(), attempt_seen.get()), (1, true));
        assert!(!spawn_probe_if_due(
            &routing(24),
            &state,
            NOW + 10,
            &mut spawn
        ));
        assert_eq!(calls.get(), 1);
    }

    fn input(harness: &str) -> HarnessInput {
        HarnessInput {
            harness: harness.to_string(),
            metered: false,
            headroom: Headroom::Known(100.0),
            new_family: Vec::new(),
            probation: Vec::new(),
            rejected: Vec::new(),
            ladder: Vec::new(),
        }
    }

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn evidence_of(harness: &str, rows: &[(&str, usize, u64)]) -> Evidence {
        Evidence {
            cells: rows
                .iter()
                .map(|(model, n, last_at)| Cell {
                    harness: harness.to_string(),
                    model: (*model).to_string(),
                    role: RouteRole::Worker,
                    complexity: CellComplexity::Any,
                    synthetic: SyntheticStats {
                        n: *n,
                        mean: Some(0.9),
                        last_at: Some(*last_at),
                        ..SyntheticStats::default()
                    },
                    real: RealStats::default(),
                })
                .collect(),
            ..Evidence::default()
        }
    }

    fn models_of(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|t| t.model.as_str()).collect()
    }

    #[test]
    fn targets_run_probation_then_stale_rejected_then_thin_or_stale_ladder() {
        let mut claude = input("claude");
        claude.probation = ids(&["p-new"]);
        claude.rejected = ids(&["r-stale", "r-fresh"]);
        claude.ladder = ids(&["l-fresh", "l-old", "l-thin"]);
        let evidence = evidence_of(
            "claude",
            &[
                ("r-stale", 9, NOW - 8 * DAY),
                ("r-fresh", 9, NOW - DAY),
                ("l-fresh", 9, NOW - DAY),
                ("l-old", 9, NOW - 8 * DAY),
                ("l-thin", 2, NOW - DAY),
            ],
        );
        let (targets, skipped) = select_targets(&routing(24), &[claude], &evidence, NOW);
        assert_eq!(
            models_of(&targets),
            vec!["p-new", "r-stale", "l-old", "l-thin"]
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn a_new_family_candidate_is_probed_before_everything_else() {
        let mut claude = input("claude");
        claude.new_family = ids(&["claude-bel-1"]);
        claude.probation = ids(&["p-new"]);
        claude.ladder = ids(&["l-thin"]);
        let (targets, _) = select_targets(&routing(24), &[claude], &Evidence::default(), NOW);
        assert_eq!(models_of(&targets), vec!["claude-bel-1", "p-new", "l-thin"]);
    }

    #[test]
    fn an_unverified_newer_version_is_a_probe_target_and_a_backed_off_one_is_not() {
        let mut promotions = Promotions::default();
        for (id, until) in [
            ("claude-mythos-6", None),
            ("claude-mythos-7", Some(NOW + 60)),
        ] {
            promotions.unverified.insert(
                id.to_string(),
                promotion::UnverifiedState {
                    vendor: "anthropic".into(),
                    first_seen: NOW,
                    unavailable_until: until,
                },
            );
        }
        let mut claude = input("claude");
        claude.new_family = promotion::unverified_candidates(&promotions, "anthropic", NOW);
        claude.ladder = ids(&["l-thin"]);
        let (targets, _) = select_targets(&routing(24), &[claude], &Evidence::default(), NOW);
        assert_eq!(models_of(&targets), vec!["claude-mythos-6", "l-thin"]);
    }

    #[test]
    fn a_candidate_whose_rows_all_failed_without_output_never_ran() {
        let row = |model: &str, status: Status, output_tokens: u64| {
            let mut row = Row::new(
                "claude",
                model,
                None,
                "t",
                super::super::corpus::Role::Worker,
                1,
                status,
            );
            row.output_tokens = output_tokens;
            row
        };
        let rows = vec![
            row("claude-bel-1", Status::Failed, 0),
            row("claude-bel-1", Status::Failed, 0),
            row("claude-bel-2", Status::Failed, 0),
            row("claude-bel-2", Status::Ok, 0),
            row("claude-bel-3", Status::Failed, 40),
            row("claude-bel-4", Status::Skipped, 0),
            row("claude-opus-5", Status::Failed, 0),
            {
                let mut hung = row("claude-bel-5", Status::Failed, 0);
                hung.exit_code = Some(crate::commands::ctx::exec::EXIT_TIMEOUT);
                hung
            },
        ];
        let candidates: BTreeSet<String> = [
            "claude-bel-1",
            "claude-bel-2",
            "claude-bel-3",
            "claude-bel-4",
            "claude-bel-5",
        ]
        .iter()
        .map(|id| (*id).to_string())
        .collect();
        assert_eq!(
            probe_outcomes(&rows, &candidates),
            vec![
                ("claude-bel-1".to_string(), false),
                ("claude-bel-2".to_string(), true),
                ("claude-bel-3".to_string(), true),
                ("claude-bel-5".to_string(), true),
            ]
        );
    }

    #[test]
    fn eligible_new_family_ids_are_reprobed_when_stale_or_thin() {
        let mut promotions = Promotions::default();
        for (id, status) in [
            ("claude-bel-1", promotion::NewFamilyStatus::Eligible),
            ("claude-bel-2", promotion::NewFamilyStatus::Eligible),
            ("claude-bel-3", promotion::NewFamilyStatus::Unavailable),
        ] {
            promotions.new_families.insert(
                id.to_string(),
                promotion::NewFamilyState {
                    vendor: "anthropic".into(),
                    family: "bel".into(),
                    first_seen: 1,
                    status,
                    unavailable_until: None,
                },
            );
        }
        let fixture = Fixture::new();
        let inputs = gather(
            &fixture.cfg,
            &fixture.state,
            NOW,
            &adapters::only_installed(&["claude"]),
            &promotions,
        );
        let claude = inputs.iter().find(|i| i.harness == "claude").unwrap();
        assert!(claude.ladder.iter().any(|m| m == "claude-bel-1"));
        assert!(!claude.ladder.iter().any(|m| m == "claude-bel-3"));
        let evidence = evidence_of(
            "claude",
            &[
                ("claude-bel-1", 9, NOW - DAY),
                ("claude-bel-2", 9, NOW - 8 * DAY),
            ],
        );
        let mut only_bel = claude.clone();
        only_bel.ladder.retain(|m| m.starts_with("claude-bel"));
        let (targets, _) = select_targets(&routing(24), &[only_bel], &evidence, NOW);
        assert_eq!(models_of(&targets), vec!["claude-bel-2"]);
    }

    #[test]
    fn targets_are_capped_at_four_per_harness_and_deduplicated() {
        let mut claude = input("claude");
        claude.probation = ids(&["a", "b", "c"]);
        claude.ladder = ids(&["a", "d", "e"]);
        let mut codex = input("codex");
        codex.ladder = ids(&["x"]);
        let (targets, _) =
            select_targets(&routing(24), &[claude, codex], &Evidence::default(), NOW);
        assert_eq!(models_of(&targets), vec!["a", "b", "c", "d", "x"]);
    }

    #[test]
    fn a_metered_harness_is_skipped_with_a_reason_unless_allowed() {
        let mut claude = input("claude");
        claude.metered = true;
        claude.ladder = ids(&["m"]);
        let (targets, skipped) = select_targets(
            &routing(24),
            std::slice::from_ref(&claude),
            &Evidence::default(),
            NOW,
        );
        assert!(targets.is_empty());
        assert!(skipped[0].reason.contains("metered"));
        let allowed = RoutingConfig {
            probe_metered: true,
            ..RoutingConfig::default()
        };
        let (targets, _) = select_targets(&allowed, &[claude], &Evidence::default(), NOW);
        assert_eq!(models_of(&targets), vec!["m"]);
    }

    #[test]
    fn a_harness_under_the_headroom_floor_or_at_its_limit_is_skipped_with_a_reason() {
        let mut low = input("claude");
        low.headroom = Headroom::Known(49.0);
        low.ladder = ids(&["m"]);
        let mut spent = input("codex");
        spent.headroom = Headroom::Known(0.0);
        spent.ladder = ids(&["m"]);
        let mut enough = input("goose");
        enough.headroom = Headroom::Known(50.0);
        enough.ladder = ids(&["m"]);
        let (targets, skipped) = select_targets(
            &routing(24),
            &[low, spent, enough],
            &Evidence::default(),
            NOW,
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].harness, "goose");
        assert!(skipped[0].reason.contains("below the 50% floor"));
        assert!(skipped[1].reason.contains("limit reached"));
    }

    #[test]
    fn unknown_headroom_is_probed_only_while_pacing_is_on() {
        let mut on = input("claude");
        on.headroom = Headroom::Unknown { pacing_on: true };
        on.ladder = ids(&["m"]);
        let mut off = input("codex");
        off.headroom = Headroom::Unknown { pacing_on: false };
        off.ladder = ids(&["m"]);
        let (targets, skipped) =
            select_targets(&routing(24), &[on, off], &Evidence::default(), NOW);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].harness, "claude");
        assert!(skipped[0].reason.contains("pacing is disabled"));
    }

    struct Fixture {
        _tmp: testenv::TestRepo,
        _home: testenv::HomeGuard,
        state: StateDir,
        cfg: CtxConfig,
        env: HashMap<&'static str, String>,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = testenv::repo();
            let home = testenv::HomeGuard::set(&tmp.path().join("home"));
            let state_dir = tmp.path().join("state");
            let env: HashMap<&'static str, String> =
                [(state::STATE_ENV, state_dir.display().to_string())].into();
            let cfg = CtxConfig::load(tmp.path(), &|key| env.get(key).cloned()).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            Self {
                state: StateDir::from_path(state_dir),
                _tmp: tmp,
                _home: home,
                cfg,
                env,
            }
        }

        /// A probation candidate for the claude harness, so there is always a target.
        fn with_probation_candidate(self) -> Self {
            let mut promotions = Promotions::default();
            promotions.families.insert(
                "anthropic.opus".to_string(),
                FamilyState {
                    incumbent: "claude-opus-5".to_string(),
                    previous: None,
                    promoted_at: None,
                    candidates: [(
                        "claude-opus-5-6".to_string(),
                        CandidateState {
                            first_seen: 1,
                            status: CandidateStatus::Probation,
                            decided_at: None,
                            last_verdict: None,
                        },
                    )]
                    .into(),
                    reason: None,
                },
            );
            std::fs::write(
                self.state.root().join(promotion::PROMOTIONS_FILE),
                serde_json::to_string(&promotions).unwrap(),
            )
            .unwrap();
            self
        }

        fn run(
            &self,
            opts: &Opts,
            installed: &'static [&'static str],
            launch: Option<&mut Launcher<'_>>,
        ) -> Result<i32, String> {
            let lookup = |key: &str| self.env.get(key).cloned();
            run(
                &self.cfg,
                &lookup,
                opts,
                false,
                &adapters::only_installed(installed),
                launch,
            )
        }

        fn log(&self) -> Vec<ProbeRecord> {
            last_records(&self.state)
        }
    }

    fn opts(force: bool, dry_run: bool) -> Opts {
        Opts {
            force,
            dry_run,
            json: false,
            quiet: true,
        }
    }

    fn fake_launch() -> run::Launch {
        run::Launch {
            exit_code: 0,
            wall_ms: 1,
            usage: Default::default(),
            cost_micros: Some(1000),
            model: None,
            answer: String::new(),
        }
    }

    #[test]
    fn with_no_targets_a_row_is_logged_and_nothing_is_stamped() {
        let fixture = Fixture::new();
        let calls = Counter::new(0);
        let mut launch = |_: &run::LaunchSpec| {
            calls.set(calls.get() + 1);
            Ok(fake_launch())
        };
        let code = fixture.run(&opts(true, false), &[], Some(&mut launch));
        assert_eq!(code, Ok(0));
        assert_eq!(calls.get(), 0);
        assert!(!fixture.state.root().join(LAST_RUN_FILE).exists());
        let log = fixture.log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].outcome, "no targets");
        assert!(log[0].run_id.is_none());
    }

    #[test]
    fn the_run_stamp_is_written_before_the_first_launch() {
        let fixture = Fixture::new().with_probation_candidate();
        let stamp_path = fixture.state.root().join(LAST_RUN_FILE);
        let calls = Counter::new(0);
        let stamped_first = Counter::new(true);
        let first_model = std::cell::RefCell::new(String::new());
        let mut launch = |spec: &run::LaunchSpec| {
            if calls.get() == 0 {
                stamped_first.set(stamp_path.exists());
                *first_model.borrow_mut() = spec.model.to_string();
            }
            calls.set(calls.get() + 1);
            Ok(fake_launch())
        };
        let code = fixture.run(&opts(false, false), &["claude"], Some(&mut launch));
        assert_eq!(code, Ok(0));
        assert!(calls.get() > 0);
        assert!(
            stamped_first.get(),
            "probe-last-run must exist at first launch"
        );
        assert_eq!(*first_model.borrow(), "claude-opus-5-6");
        let log = fixture.log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].outcome, "ran");
        assert_eq!(
            log[0].spend_micros,
            1000 * u64::try_from(calls.get()).unwrap()
        );
        assert!(log[0].run_id.is_some());
    }

    #[test]
    fn a_probe_that_is_not_due_does_nothing_without_force() {
        let fixture = Fixture::new().with_probation_candidate();
        stamp(&fixture.state, LAST_RUN_FILE, state::now_secs());
        let calls = Counter::new(0);
        let mut launch = |_: &run::LaunchSpec| {
            calls.set(calls.get() + 1);
            Ok(fake_launch())
        };
        let code = fixture.run(&opts(false, false), &["claude"], Some(&mut launch));
        assert_eq!(code, Ok(0));
        assert_eq!(calls.get(), 0);
        assert!(fixture.log().is_empty());
    }

    #[test]
    fn the_child_proceeds_past_the_gate_after_the_spawner_stamps_an_attempt() {
        let fixture = Fixture::new().with_probation_candidate();
        let now = state::now_secs();
        stamp(&fixture.state, ATTEMPT_FILE, now);
        let calls = Counter::new(0);
        let mut launch = |_: &run::LaunchSpec| {
            calls.set(calls.get() + 1);
            Ok(fake_launch())
        };
        let code = fixture.run(&opts(false, false), &["claude"], Some(&mut launch));
        assert_eq!(code, Ok(0));
        assert!(calls.get() > 0, "a fresh attempt stamp must not gate run");
    }

    #[test]
    fn a_busy_lock_exits_zero_without_probing() {
        let fixture = Fixture::new().with_probation_candidate();
        let _held = state::try_acquire_lock(&fixture.state.root().join(LOCK_FILE)).unwrap();
        let calls = Counter::new(0);
        let mut launch = |_: &run::LaunchSpec| {
            calls.set(calls.get() + 1);
            Ok(fake_launch())
        };
        let code = fixture.run(&opts(true, false), &["claude"], Some(&mut launch));
        assert_eq!(code, Ok(0));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn a_dry_run_launches_nothing_and_writes_nothing() {
        let fixture = Fixture::new().with_probation_candidate();
        let calls = Counter::new(0);
        let mut launch = |_: &run::LaunchSpec| {
            calls.set(calls.get() + 1);
            Ok(fake_launch())
        };
        let code = fixture.run(&opts(false, true), &["claude"], Some(&mut launch));
        assert_eq!(code, Ok(0));
        assert_eq!(calls.get(), 0);
        assert!(!fixture.state.root().join(LAST_RUN_FILE).exists());
        assert!(!fixture.state.root().join("benchmark").exists());
        assert!(fixture.log().is_empty());
    }

    #[test]
    fn the_status_section_names_the_last_run_skips_and_spend() {
        let fixture = Fixture::new();
        assert!(render_probe_status(&fixture.state, &fixture.cfg, NOW).contains("never"));
        stamp(&fixture.state, LAST_RUN_FILE, NOW - 3600);
        append_log(
            &fixture.state,
            &ProbeRecord {
                ts: NOW - 3600,
                run_id: Some("r1".to_string()),
                targets: vec![Target {
                    harness: "claude".to_string(),
                    model: "m".to_string(),
                }],
                skipped: vec![Skip {
                    harness: "codex".to_string(),
                    reason: "metered".to_string(),
                }],
                spend_micros: 1_230_000,
                outcome: "ran".to_string(),
            },
        );
        let text = render_probe_status(&fixture.state, &fixture.cfg, NOW);
        assert!(text.contains("codex: metered"), "{text}");
        assert!(text.contains("$1.23"), "{text}");
        assert!(!text.contains("never"), "{text}");
    }
}
