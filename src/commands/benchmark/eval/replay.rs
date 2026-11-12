//! Paid replay: run each case through a real headless harness with every tool call denied, grade
//! the recorded trace, and compare against a baseline run. The launcher is injected, so tests
//! never start a harness.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use super::corpus::Case;
use super::grade::{Check, grade};
use super::trace::trace;
use crate::commands::benchmark::run::{self, Launch, LaunchSpec, Status};
use crate::commands::ctx::state;
use crate::commands::workflow::research::backend::{
    Money, SpendReport, TRIAL_SPEC_SCHEMA, TrialResult, TrialStatus,
};

const ROW_SCHEMA: u32 = 1;
const STOPPED_REASON: &str = "stopped on regression";

pub type Launcher<'a> = dyn FnMut(&LaunchSpec) -> Result<Launch, String> + 'a;

/// What `run` needs besides the cases; paths are resolved by the caller.
pub struct Opts<'a> {
    pub samples: u32,
    pub max_usd: f64,
    pub baseline: Option<&'a str>,
    pub stop_on_regression: bool,
    /// `<state>/eval`.
    pub root: &'a Path,
    pub work: &'a Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    schema: u32,
    run_id: String,
    started_at: String,
    zirv_version: String,
    cases: Vec<String>,
    samples: u32,
    max_usd: f64,
    baseline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Row {
    schema: u32,
    case: String,
    harness: String,
    sample: u32,
    status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skip_reason: Option<String>,
    passed: bool,
    checks: Vec<Check>,
    bundle_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wall_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trace_file: Option<String>,
}

impl Row {
    fn new(case: &Case, bundle: &str, sample: u32, status: Status) -> Self {
        Self {
            schema: ROW_SCHEMA,
            case: case.id.clone(),
            harness: case.harness().to_string(),
            sample,
            status,
            skip_reason: None,
            passed: false,
            checks: Vec::new(),
            bundle_sha256: bundle.to_string(),
            exit_code: None,
            wall_ms: None,
            cost_micros: None,
            error: None,
            trace_file: None,
        }
    }

    fn skipped(case: &Case, bundle: &str, sample: u32, reason: &str) -> Self {
        let mut row = Self::new(case, bundle, sample, Status::Skipped);
        row.skip_reason = Some(reason.to_string());
        row
    }

    fn failed(mut self, error: String) -> Self {
        self.status = Status::Failed;
        self.error = Some(error);
        self
    }
}

/// One case's outcome: it fails when at least half of its counted samples fail.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub case: String,
    pub samples: u32,
    pub failed: u32,
    pub passed: bool,
    pub bundle_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Regression,
    Improved,
    Unchanged,
    /// The baseline has no verdict for this case.
    New,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub case: String,
    pub change: Change,
    pub baseline_passed: Option<bool>,
    pub passed: bool,
    /// The resolved surface text differs from the baseline's, so the two runs are not like for like.
    pub surface_changed: bool,
    pub baseline_bundle_sha256: Option<String>,
    pub bundle_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    run_id: String,
    started_at: String,
    baseline: Option<String>,
    /// Set when the stored baseline run could not be loaded.
    baseline_error: Option<String>,
    samples: u32,
    max_usd: f64,
    spend_micros: u64,
    unpriced_samples: u32,
    skipped_samples: u32,
    verdicts: Vec<Verdict>,
    comparisons: Vec<Comparison>,
    regressions: usize,
    surface_changed: usize,
}

/// Verdicts in first-seen case order; a case with no counted sample (all skipped) has none.
fn verdicts(rows: &[Row]) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = Vec::new();
    for row in rows.iter().filter(|r| r.status != Status::Skipped) {
        let index = match out.iter().position(|v| v.case == row.case) {
            Some(index) => index,
            None => {
                out.push(Verdict {
                    case: row.case.clone(),
                    samples: 0,
                    failed: 0,
                    passed: true,
                    bundle_sha256: row.bundle_sha256.clone(),
                });
                out.len() - 1
            }
        };
        out[index].samples += 1;
        out[index].failed += u32::from(!row.passed);
    }
    for verdict in &mut out {
        verdict.passed = verdict.failed * 2 < verdict.samples;
    }
    out
}

fn compare_case(base: Option<&Verdict>, now: &Verdict) -> Comparison {
    let change = match base {
        None => Change::New,
        Some(b) if b.passed && !now.passed => Change::Regression,
        Some(b) if !b.passed && now.passed => Change::Improved,
        Some(_) => Change::Unchanged,
    };
    Comparison {
        case: now.case.clone(),
        change,
        baseline_passed: base.map(|b| b.passed),
        passed: now.passed,
        surface_changed: base.is_some_and(|b| b.bundle_sha256 != now.bundle_sha256),
        baseline_bundle_sha256: base.map(|b| b.bundle_sha256.clone()),
        bundle_sha256: now.bundle_sha256.clone(),
    }
}

fn compare(base: &[Verdict], now: &[Verdict]) -> Vec<Comparison> {
    now.iter()
        .map(|v| compare_case(base.iter().find(|b| b.case == v.case), v))
        .collect()
}

fn build_report(
    meta: &Meta,
    rows: &[Row],
    baseline: Option<&[Verdict]>,
    error: Option<String>,
) -> Report {
    let verdicts = verdicts(rows);
    let comparisons = baseline
        .map(|base| compare(base, &verdicts))
        .unwrap_or_default();
    Report {
        run_id: meta.run_id.clone(),
        started_at: meta.started_at.clone(),
        baseline: meta.baseline.clone(),
        baseline_error: error,
        samples: meta.samples,
        max_usd: meta.max_usd,
        spend_micros: rows.iter().filter_map(|r| r.cost_micros).sum(),
        unpriced_samples: rows
            .iter()
            .filter(|r| r.status != Status::Skipped && r.cost_micros.is_none())
            .count() as u32,
        skipped_samples: rows.iter().filter(|r| r.status == Status::Skipped).count() as u32,
        regressions: comparisons
            .iter()
            .filter(|c| c.change == Change::Regression)
            .count(),
        surface_changed: comparisons.iter().filter(|c| c.surface_changed).count(),
        verdicts,
        comparisons,
    }
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn render(report: &Report) -> String {
    let mut text = format!(
        "eval run {}  started {}  samples {}  cap ${:.2}\n",
        report.run_id, report.started_at, report.samples, report.max_usd
    );
    for v in &report.verdicts {
        text.push_str(&format!(
            "{:<4} {:<36} {}/{} samples passed  bundle {}\n",
            if v.passed { "PASS" } else { "FAIL" },
            v.case,
            v.samples - v.failed,
            v.samples,
            short(&v.bundle_sha256)
        ));
    }
    if report.skipped_samples > 0 {
        text.push_str(&format!("{} sample(s) skipped\n", report.skipped_samples));
    }
    text.push_str(&format!(
        "spend ${:.4} measured ({} sample(s) with unknown cost)\n",
        report.spend_micros as f64 / 1_000_000.0,
        report.unpriced_samples
    ));
    if let Some(error) = &report.baseline_error {
        text.push_str(&format!("baseline not comparable: {error}\n"));
    }
    let Some(baseline) = &report.baseline else {
        return text;
    };
    text.push_str(&format!("\nagainst baseline {baseline}\n"));
    for c in &report.comparisons {
        let label = match c.change {
            Change::Regression => "REGRESSION",
            Change::Improved => "improved",
            Change::Unchanged => "unchanged",
            Change::New => "new",
        };
        text.push_str(&format!("{label:<10} {}\n", c.case));
        if c.surface_changed {
            text.push_str(&format!(
                "           surface changed: bundle {} -> {}\n",
                c.baseline_bundle_sha256.as_deref().map_or("?", short),
                short(&c.bundle_sha256)
            ));
        }
    }
    text.push_str(&format!(
        "{} regression(s), {} case(s) with a changed surface\n",
        report.regressions, report.surface_changed
    ));
    text
}

fn load(dir: &Path) -> Result<(Meta, Vec<Row>), String> {
    let path = dir.join("run.json");
    let meta = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta: Meta = serde_json::from_str(&meta).map_err(|e| format!("run.json: {e}"))?;
    // A crashed run may end on a partial line; every complete row still counts.
    let rows = std::fs::read_to_string(dir.join("results.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Ok((meta, rows))
}

fn load_baseline(root: &Path, id: &str) -> Result<Vec<Verdict>, String> {
    let (_, rows) = load(&run::find_run(root, Some(id))?)?;
    Ok(verdicts(&rows))
}

/// Setup files go under the work directory; a path that could leave it is refused.
fn write_setup(work: &Path, case: &Case) -> std::io::Result<()> {
    for (relative, contents) in &case.setup.files {
        let path = Path::new(relative);
        if path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
            return Err(std::io::Error::other(format!(
                "setup file '{relative}' must stay inside the workdir"
            )));
        }
        let target = work.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, contents)?;
    }
    Ok(())
}

fn prompt(case: &Case) -> String {
    let mut parts: Vec<String> = case
        .setup
        .prior_turns
        .iter()
        .map(|turn| format!("Earlier in this session the user said: {turn}"))
        .collect();
    parts.extend(case.turns.iter().cloned());
    parts.join("\n\n")
}

fn sample(
    case: &Case,
    bundle: &str,
    index: u32,
    opts: &Opts<'_>,
    dir: &Path,
    launch: &mut Launcher<'_>,
) -> Row {
    let mut row = Row::new(case, bundle, index, Status::Ok);
    let reset = run::reset_dir(opts.work, false, &|work| write_setup(work, case));
    if let Err(error) = reset {
        return row.failed(error);
    }
    let spec = LaunchSpec {
        harness: case.harness(),
        model: crate::commands::benchmark::discover::DEFAULT_MODEL,
        prompt: prompt(case),
        read_only: false,
        deny_all: true,
    };
    let launched = match launch(&spec) {
        Ok(launched) => launched,
        Err(error) => return row.failed(error),
    };
    row.exit_code = Some(launched.exit_code);
    row.wall_ms = Some(launched.wall_ms);
    row.cost_micros = launched.cost_micros;
    if launched.exit_code != 0 {
        return row.failed(format!("harness exited {}", launched.exit_code));
    }
    if launched.transcript.is_empty() {
        return row.failed("the harness left no transcript to grade".to_string());
    }
    let relative = format!("traces/{}-{index}.jsonl", case.id);
    let saved = state::create_private_dir_all(&dir.join("traces"))
        .and_then(|()| state::write_private(&dir.join(&relative), &launched.transcript));
    if saved.is_ok() {
        row.trace_file = Some(relative);
    }
    let traced = match trace(case.harness(), &launched.transcript) {
        Ok(traced) => traced,
        Err(error) => return row.failed(error),
    };
    let graded = grade(&case.expect, &traced);
    row.passed = graded.passed();
    row.checks = graded.checks;
    row
}

/// `cases` carries each case with its resolved bundle hash.
pub fn collect(
    cases: &[(&Case, String)],
    opts: &Opts<'_>,
    launch: &mut Launcher<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<Report, String> {
    let baseline = opts
        .baseline
        .map(|id| load_baseline(opts.root, id))
        .transpose()?;
    let started = chrono::Utc::now();
    let meta = Meta {
        schema: ROW_SCHEMA,
        run_id: run::run_id(started),
        started_at: started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        zirv_version: env!("CARGO_PKG_VERSION").to_string(),
        cases: cases.iter().map(|(c, _)| c.id.clone()).collect(),
        samples: opts.samples,
        max_usd: opts.max_usd,
        baseline: opts.baseline.map(str::to_string),
    };
    let dir = opts.root.join(&meta.run_id);
    run::start_store(&dir, &meta)?;
    let cap_micros = (opts.max_usd * 1_000_000.0).round() as u64;
    let mut spent: u64 = 0;
    let mut rows: Vec<Row> = Vec::new();
    let mut stopped = false;
    for (case, bundle) in cases {
        let mut mine: Vec<Row> = Vec::new();
        let mut index = 0;
        loop {
            let counted = mine.iter().filter(|r| r.status != Status::Skipped).count() as u32;
            let failures = mine
                .iter()
                .filter(|r| r.status != Status::Skipped && !r.passed)
                .count() as u32;
            // One more sample breaks an even tie, e.g. the first two of two disagreeing.
            let tied =
                counted == opts.samples && counted.is_multiple_of(2) && failures * 2 == counted;
            if index >= opts.samples && !(tied && index == opts.samples) {
                break;
            }
            let row = if stopped {
                Row::skipped(case, bundle, index, STOPPED_REASON)
            } else if spent >= cap_micros {
                Row::skipped(case, bundle, index, run::SPEND_CAP_REASON)
            } else {
                sample(case, bundle, index, opts, &dir, launch)
            };
            spent = spent.saturating_add(row.cost_micros.unwrap_or(0));
            progress(&format!(
                "{} sample {}: {}",
                case.id,
                index + 1,
                match row.status {
                    Status::Skipped => "skipped",
                    _ if row.passed => "pass",
                    _ => "fail",
                }
            ));
            run::append_row(&dir, &row)?;
            mine.push(row);
            index += 1;
        }
        if let (Some(base), false) = (&baseline, stopped)
            && opts.stop_on_regression
            && let Some(now) = verdicts(&mine).first()
            && compare_case(base.iter().find(|b| b.case == now.case), now).change
                == Change::Regression
        {
            stopped = true;
        }
        rows.append(&mut mine);
    }
    Ok(build_report(&meta, &rows, baseline.as_deref(), None))
}

/// Prints the report. Returns the exit code: 1 when a regression against the baseline was found,
/// otherwise 0.
pub fn finish(report: &Report, json: bool, out: &mut dyn std::io::Write) -> Result<i32, String> {
    super::super::emit(out, json, report, &render(report))?;
    Ok(i32::from(report.regressions > 0))
}

impl Report {
    /// The research trial for a run over `selected` cases. Score (`correctness`) is the share of
    /// selected cases that passed; a case with no counted sample (spend cap reached) counts as not
    /// passed. A run with no counted case, or whose measured spend exceeds `ceiling_usd`, is an
    /// error trial: the single-sample overshoot past the cap is reported, never hidden.
    pub fn trial_result(
        &self,
        trial_id: &str,
        selected: usize,
        ceiling_usd: f64,
        wall_ms: u64,
        route: serde_json::Value,
    ) -> TrialResult {
        let passed = self.verdicts.iter().filter(|v| v.passed).count();
        let counted: u32 = self.verdicts.iter().map(|v| v.samples).sum();
        let over = self.spend_micros as f64 > ceiling_usd * 1_000_000.0;
        let details = if self.verdicts.is_empty() {
            Some("no eval case was graded".to_string())
        } else if over {
            Some(format!(
                "measured spend ${:.4} exceeds the per-trial ceiling ${ceiling_usd:.4}",
                self.spend_micros as f64 / 1_000_000.0
            ))
        } else {
            None
        };
        let usd = self.spend_micros as f64 / 1_000_000.0;
        TrialResult {
            schema: TRIAL_SPEC_SCHEMA,
            trial_id: trial_id.to_string(),
            status: if details.is_some() {
                TrialStatus::Error
            } else {
                TrialStatus::Ok
            },
            correctness: (!self.verdicts.is_empty())
                .then(|| passed as f64 / selected.max(1) as f64),
            quality: None,
            wall_ms,
            spend: Some(SpendReport {
                execution: Money {
                    reported_usd: Some(usd),
                    estimated_usd: None,
                    unknown_count: u64::from(self.unpriced_samples),
                    calls: u64::from(counted),
                },
                overhead: Money::default(),
                calls: u64::from(counted),
                completeness: if self.unpriced_samples == 0 {
                    "complete"
                } else {
                    "partial"
                }
                .to_string(),
                receipts: BTreeMap::from([
                    ("eval:cases_passed".to_string(), passed as u64),
                    ("eval:cases_failed".to_string(), (selected - passed) as u64),
                    ("eval:samples".to_string(), u64::from(counted)),
                    (
                        "eval:samples_skipped".to_string(),
                        u64::from(self.skipped_samples),
                    ),
                ]),
            }),
            route: Some(route),
            env_fingerprint: None,
            details,
        }
    }
}

/// Re-render a stored run (the newest when `run_id` is `None`), compared against its baseline.
pub fn report(
    root: &Path,
    run_id: Option<&str>,
    json: bool,
    out: &mut dyn std::io::Write,
) -> Result<i32, String> {
    let (meta, rows) = load(&run::find_run(root, run_id)?)?;
    let (baseline, error) = match meta.baseline.as_deref().map(|id| load_baseline(root, id)) {
        Some(Ok(verdicts)) => (Some(verdicts), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    let report = build_report(&meta, &rows, baseline.as_deref(), error);
    super::super::emit(out, json, &report, &render(&report))?;
    Ok(0)
}

/// The plan a missing `--yes` prints: nothing here launches anything.
pub fn render_plan(cases: &[(&Case, String)], samples: u32, max_usd: f64) -> String {
    let mut by_harness: BTreeMap<&str, usize> = BTreeMap::new();
    let mut text = String::new();
    for (case, bundle) in cases {
        *by_harness.entry(case.harness()).or_default() += 1;
        text.push_str(&format!(
            "{:<36} {:<7} bundle {}\n",
            case.id,
            case.harness(),
            short(bundle)
        ));
    }
    let tiebreak = u32::from(samples.is_multiple_of(2));
    text.push_str(&format!(
        "\n{} case(s), {samples} sample(s) each (up to {} with a tie-break), {} to {} harness runs, cap ${max_usd:.2}\n\
         every tool call is denied; the transcript records the attempt and nothing runs\n",
        cases.len(),
        samples + tiebreak,
        cases.len() as u32 * samples,
        cases.len() as u32 * (samples + tiebreak),
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(case: &str, sample: u32, passed: bool, bundle: &str) -> Row {
        Row {
            schema: ROW_SCHEMA,
            case: case.to_string(),
            harness: "claude".to_string(),
            sample,
            status: Status::Ok,
            skip_reason: None,
            passed,
            checks: Vec::new(),
            bundle_sha256: bundle.to_string(),
            exit_code: Some(0),
            wall_ms: None,
            cost_micros: None,
            error: None,
            trace_file: None,
        }
    }

    #[test]
    fn a_case_fails_when_at_least_half_of_its_counted_samples_fail() {
        let one_of_two = [row("a", 0, true, "h"), row("a", 1, false, "h")];
        assert!(!verdicts(&one_of_two)[0].passed);
        let one_of_three = [
            row("a", 0, true, "h"),
            row("a", 1, false, "h"),
            row("a", 2, true, "h"),
        ];
        assert!(verdicts(&one_of_three)[0].passed);
        let mut skipped = row("a", 1, false, "h");
        skipped.status = Status::Skipped;
        let verdict = &verdicts(&[row("a", 0, true, "h"), skipped])[0];
        assert_eq!((verdict.samples, verdict.passed), (1, true));
    }

    #[test]
    fn a_fully_skipped_case_has_no_verdict() {
        let mut skipped = row("a", 0, false, "h");
        skipped.status = Status::Skipped;
        assert!(verdicts(&[skipped]).is_empty());
    }

    #[test]
    fn comparison_names_regressions_improvements_and_a_changed_surface() {
        let base = verdicts(&[
            row("kept", 0, true, "h1"),
            row("lost", 0, true, "h1"),
            row("won", 0, false, "h1"),
        ]);
        let now = verdicts(&[
            row("kept", 0, true, "h1"),
            row("lost", 0, false, "h2"),
            row("won", 0, true, "h1"),
            row("fresh", 0, true, "h1"),
        ]);
        let changes: Vec<_> = compare(&base, &now)
            .into_iter()
            .map(|c| (c.case, c.change, c.surface_changed))
            .collect();
        assert_eq!(
            changes,
            [
                ("kept".to_string(), Change::Unchanged, false),
                ("lost".to_string(), Change::Regression, true),
                ("won".to_string(), Change::Improved, false),
                ("fresh".to_string(), Change::New, false),
            ]
        );
    }

    #[test]
    fn setup_files_cannot_escape_the_workdir() {
        let tmp = tempfile::tempdir().unwrap();
        let cases = super::super::corpus::embedded().unwrap();
        let mut case = cases[0].clone();
        case.setup.files.clear();
        case.setup
            .files
            .insert("../outside.txt".to_string(), "x".to_string());
        assert!(write_setup(tmp.path(), &case).is_err());
        case.setup.files.clear();
        case.setup
            .files
            .insert("dir/ok.txt".to_string(), "x".to_string());
        write_setup(tmp.path(), &case).unwrap();
        assert!(tmp.path().join("dir/ok.txt").is_file());
    }
}
