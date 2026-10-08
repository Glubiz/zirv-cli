//! Planning and sequential execution of a benchmark, and its append-only storage.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::Filters;
use super::corpus::{self, Role, Task};
use super::discover::{self, Candidate, DEFAULT_MODEL, HarnessInfo, Judge};
use super::report::usd_amount;
use crate::commands::ctx::adapters::{self, Liveness};
use crate::commands::ctx::agent;
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::event::{SessionId, SessionRef, TranscriptUsage};
use crate::commands::ctx::exec::{self, ExecArgs};
use crate::commands::ctx::models::Listing;
use crate::commands::ctx::price::{self, PriceTable};
use crate::commands::ctx::sessions::SUPERVISION_ENV;
use crate::commands::ctx::state;

const ROW_SCHEMA: u32 = 1;
const DIFF_LIMIT: usize = 20 * 1024;
const SPEND_CAP_REASON: &str = "spend cap";

const NOTICE: &str = "You are running non-interactively: nobody will answer questions or approve plans. Make reasonable decisions yourself and complete the task end to end.\n\n";

/// Keeps zirv-side extra model calls and rerouting out of the measurement.
pub const CHILD_KNOBS: [(&str, &str); 4] = [
    ("ZIRV_CTX_SUPERVISOR_ENABLED", "false"),
    ("ZIRV_CTX_MEMORY_HARVEST", "false"),
    ("ZIRV_CTX_PACE", "false"),
    ("ZIRV_CTX_FALLBACK", "false"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub schema: u32,
    pub harness: String,
    /// The model the run actually used: the transcript's, once a launch succeeded.
    pub model: String,
    /// The planned candidate's label (e.g. `default`); empty in rows from before this field.
    #[serde(default)]
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub task: String,
    pub role: Role,
    pub rep: u32,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    #[serde(default)]
    pub graders_passed: u32,
    #[serde(default)]
    pub graders_evaluated: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correctness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_file: Option<String>,
    /// Why a run that never produced an exit code failed to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Row {
    /// The planned candidate this run belongs to, falling back to `model` for old rows.
    pub fn candidate_label(&self) -> &str {
        if self.candidate.is_empty() {
            &self.model
        } else {
            &self.candidate
        }
    }

    pub fn new(
        harness: &str,
        model: &str,
        family: Option<&str>,
        task: &str,
        role: Role,
        rep: u32,
        status: Status,
    ) -> Self {
        Self {
            schema: ROW_SCHEMA,
            harness: harness.to_string(),
            model: model.to_string(),
            candidate: model.to_string(),
            family: family.map(str::to_string),
            task: task.to_string(),
            role,
            rep,
            status,
            skip_reason: None,
            exit_code: None,
            wall_ms: None,
            input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 0,
            cost_micros: None,
            graders_passed: 0,
            graders_evaluated: 0,
            correctness: None,
            judge_score: None,
            judge_cost_micros: None,
            judge_error: None,
            answer_file: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedTask {
    pub id: String,
    pub reason: String,
}

/// `run.json`: what was benchmarked, written before the first run starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub schema: u32,
    pub run_id: String,
    pub started_at: String,
    pub zirv_version: String,
    pub candidates: Vec<Candidate>,
    pub tasks: Vec<String>,
    pub skipped_tasks: Vec<SkippedTask>,
    pub reps: u32,
    pub judge: Option<Judge>,
    pub max_usd: f64,
    pub prices_as_of: String,
    pub python3: bool,
    #[serde(default)]
    pub judge_unpriced: bool,
}

#[cfg(test)]
impl RunMeta {
    pub fn for_test() -> Self {
        Self {
            schema: 1,
            run_id: "20260101T000000Z0000".to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            zirv_version: "0.0.0".to_string(),
            candidates: Vec::new(),
            tasks: Vec::new(),
            skipped_tasks: Vec::new(),
            reps: 1,
            judge: None,
            max_usd: 10.0,
            prices_as_of: "2026-01-01".to_string(),
            python3: true,
            judge_unpriced: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub harnesses: Vec<HarnessInfo>,
    pub candidates: Vec<Candidate>,
    pub task_ids: Vec<String>,
    pub skipped_tasks: Vec<SkippedTask>,
    pub judge: Option<Judge>,
    pub reps: u32,
    pub agent_runs: usize,
    pub judge_calls: usize,
    /// The default judge is absent because no candidate has a known output price.
    pub judge_unpriced: bool,
    pub max_usd: f64,
    pub prices_as_of: String,
    pub python3: bool,
    #[serde(skip)]
    pub tasks: Vec<Task>,
}

impl Plan {
    pub fn meta(&self, run_id: &str, started_at: &str) -> RunMeta {
        RunMeta {
            schema: ROW_SCHEMA,
            run_id: run_id.to_string(),
            started_at: started_at.to_string(),
            zirv_version: env!("CARGO_PKG_VERSION").to_string(),
            candidates: self.candidates.clone(),
            tasks: self.task_ids.clone(),
            skipped_tasks: self.skipped_tasks.clone(),
            reps: self.reps,
            judge: self.judge.clone(),
            max_usd: self.max_usd,
            prices_as_of: self.prices_as_of.clone(),
            python3: self.python3,
            judge_unpriced: self.judge_unpriced,
        }
    }
}

/// Read-only: no model call, no estimate; every number is a count or comes from config.
pub fn plan(
    cfg: &CtxConfig,
    filters: &Filters,
    reps: u32,
    max_usd: f64,
    python_present: bool,
    present: &dyn Fn(&str, &str) -> Liveness,
    listing: &Listing,
) -> Result<Plan, String> {
    let corpus = corpus::embedded()?;
    for id in &filters.tasks {
        if !corpus.tasks.iter().any(|task| &task.id == id) {
            return Err(format!("unknown task '{id}'"));
        }
    }
    let discovery = discover::discover(cfg, filters, present, listing)?;

    let mut tasks = Vec::new();
    let mut skipped_tasks = Vec::new();
    for task in corpus
        .tasks
        .into_iter()
        .filter(|task| filters.tasks.is_empty() || filters.tasks.contains(&task.id))
    {
        match corpus::skip_reason(&task, python_present) {
            Some(reason) => skipped_tasks.push(SkippedTask {
                id: task.id,
                reason: reason.to_string(),
            }),
            None => tasks.push(task),
        }
    }

    let per_candidate = tasks.len() * reps as usize;
    let judged = tasks.iter().filter(|task| task.judge).count() * reps as usize;
    Ok(Plan {
        agent_runs: discovery.candidates.len() * per_candidate,
        judge_calls: if discovery.judge.is_some() {
            discovery.candidates.len() * judged
        } else {
            0
        },
        judge_unpriced: discovery.judge.is_none()
            && !filters.no_judge
            && !discovery.candidates.is_empty(),
        task_ids: tasks.iter().map(|task| task.id.clone()).collect(),
        harnesses: discovery.harnesses,
        candidates: discovery.candidates,
        skipped_tasks,
        judge: discovery.judge,
        reps,
        max_usd,
        prices_as_of: price::resolve_table(cfg).as_of,
        python3: python_present,
        tasks,
    })
}

pub fn render_plan(plan: &Plan) -> String {
    let mut out = String::from("harnesses\n");
    for harness in &plan.harnesses {
        out.push_str(&format!("  {:<14} {:?}\n", harness.name, harness.presence).to_lowercase());
    }
    out.push_str("\ncandidates\n");
    if plan.candidates.is_empty() {
        out.push_str("  none\n");
    }
    for c in &plan.candidates {
        out.push_str(&format!(
            "  {:<14} {:<32} family {}\n",
            c.harness,
            c.model,
            c.family.as_deref().unwrap_or("-")
        ));
    }
    out.push_str(
        "  source: models discovered on this machine, see `zirv ctx models`; refresh with `zirv ctx models refresh`\n",
    );
    out.push_str("\ntasks\n");
    for task in &plan.tasks {
        out.push_str(&format!("  {:<12} {}\n", task.id, task.role.as_str()));
    }
    for skipped in &plan.skipped_tasks {
        out.push_str(&format!(
            "  {:<12} skipped: {}\n",
            skipped.id, skipped.reason
        ));
    }
    match &plan.judge {
        Some(judge) => {
            out.push_str(&format!("\njudge: {}/{}\n", judge.harness, judge.model));
            if judge.also_candidate {
                out.push_str(
                    "  note: the judge is also a candidate; judges can favour their own output\n",
                );
            }
        }
        None if plan.judge_unpriced => out.push_str(
            "\njudge: none (no candidate has a known output price; pass --judge <harness:model>)\n",
        ),
        None => out.push_str("\njudge: none\n"),
    }
    out.push_str(&format!(
        "\nruns: {} agent run(s) ({} candidate(s) x {} task(s) x {} rep(s)) + {} judge call(s)\n",
        plan.agent_runs,
        plan.candidates.len(),
        plan.tasks.len(),
        plan.reps,
        plan.judge_calls
    ));
    out.push_str(&format!(
        "spend cap: {}  prices as of {}\n",
        usd_amount(plan.max_usd),
        plan.prices_as_of
    ));
    out.push_str(
        "cost: each run's transcript tokens x the price table; unknown when a harness's transcript \
carries no token counts or the model has no price\n\
note: runs with unknown cost are not counted against the spend cap\n",
    );
    out
}

pub fn run_id(started: chrono::DateTime<chrono::Utc>) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{}{}", started.format("%Y%m%dT%H%M%SZ"), &suffix[..4])
}

/// One launch of a harness, as the runner needs it.
pub struct LaunchSpec<'a> {
    pub harness: &'a str,
    pub model: &'a str,
    pub prompt: String,
    pub read_only: bool,
}

pub struct Launch {
    pub exit_code: i32,
    pub wall_ms: u64,
    pub usage: TranscriptUsage,
    pub cost_micros: Option<u64>,
    /// The model the transcript names, when it does.
    pub model: Option<String>,
    pub answer: String,
}

pub struct Runner<'a> {
    pub dir: &'a Path,
    pub work: &'a Path,
    pub python_present: bool,
    pub git_present: bool,
    pub launch: &'a mut dyn FnMut(&LaunchSpec) -> Result<Launch, String>,
    pub progress: &'a mut dyn FnMut(&str),
}

impl Runner<'_> {
    /// Sequential on purpose: parallel runs distort wall time and trip rate limits.
    /// Repetitions are the outer loop so a spend cap leaves every candidate equally covered.
    pub fn execute(&mut self, plan: &Plan) -> Result<Vec<Row>, String> {
        let rows = self.execute_all(plan);
        let _ = std::fs::remove_dir_all(self.work);
        rows
    }

    fn execute_all(&mut self, plan: &Plan) -> Result<Vec<Row>, String> {
        let cap_micros = (plan.max_usd * 1_000_000.0).round() as u64;
        let mut spent: u64 = 0;
        let mut rows = Vec::new();
        let tasks = interleave_roles(&plan.tasks);
        for rep in 0..plan.reps {
            for task in &tasks {
                for candidate in &plan.candidates {
                    let row = if spent >= cap_micros {
                        let mut row = base_row(candidate, task, rep, Status::Skipped);
                        row.skip_reason = Some(SPEND_CAP_REASON.to_string());
                        row
                    } else {
                        let index = rows.len();
                        self.run_one(plan, candidate, task, rep, index)
                    };
                    spent = spent
                        .saturating_add(row.cost_micros.unwrap_or(0))
                        .saturating_add(row.judge_cost_micros.unwrap_or(0));
                    (self.progress)(&format!(
                        "[{}/{}] {}/{} {} rep {}: {:?}",
                        rows.len() + 1,
                        plan.agent_runs,
                        candidate.harness,
                        candidate.model,
                        task.id,
                        rep + 1,
                        row.status
                    ));
                    append_row(self.dir, &row)?;
                    rows.push(row);
                }
            }
        }
        Ok(rows)
    }

    fn run_one(
        &mut self,
        plan: &Plan,
        candidate: &Candidate,
        task: &Task,
        rep: u32,
        index: usize,
    ) -> Row {
        let mut row = base_row(candidate, task, rep, Status::Ok);
        if let Err(error) = self.reset_workdir() {
            return failed_to_start(row, error);
        }
        let spec = LaunchSpec {
            harness: &candidate.harness,
            model: &candidate.model,
            prompt: format!("{NOTICE}{}", task.prompt),
            read_only: false,
        };
        let launched = match (self.launch)(&spec) {
            Ok(launched) => launched,
            Err(error) => return failed_to_start(row, error),
        };
        row.exit_code = Some(launched.exit_code);
        row.wall_ms = Some(launched.wall_ms);
        row.input_tokens = launched.usage.input_tokens;
        row.cache_creation_input_tokens = launched.usage.cache_creation_input_tokens;
        row.cache_read_input_tokens = launched.usage.cache_read_input_tokens;
        row.output_tokens = launched.usage.output_tokens;
        row.cost_micros = launched.cost_micros;
        if let Some(model) = launched.model {
            row.model = model;
        }
        row.answer_file = self.save_answer(index, &launched.answer);
        if launched.exit_code != 0 {
            row.status = Status::Failed;
            row.correctness = Some(0.0);
            return row;
        }
        let grade = corpus::grade(task, &launched.answer, self.work, self.python_present);
        row.graders_passed = grade.passed;
        row.graders_evaluated = grade.evaluated;
        row.correctness = grade.correctness();
        if let (Some(judge), true) = (&plan.judge, task.judge) {
            self.judge(judge, task, &launched.answer, &mut row);
        }
        row
    }

    fn judge(&mut self, judge: &Judge, task: &Task, answer: &str, row: &mut Row) {
        let diff = if self.git_present {
            git_diff(self.work)
        } else {
            String::new()
        };
        let spec = LaunchSpec {
            harness: &judge.harness,
            model: &judge.model,
            prompt: judge_prompt(&task.prompt, answer, &diff),
            read_only: true,
        };
        match (self.launch)(&spec) {
            Err(error) => row.judge_error = Some(error),
            Ok(launched) => {
                row.judge_cost_micros = launched.cost_micros;
                if launched.exit_code != 0 {
                    row.judge_error = Some(format!("judge exited {}", launched.exit_code));
                    return;
                }
                match parse_judge_score(&launched.answer) {
                    Ok(score) => row.judge_score = Some(score),
                    Err(error) => row.judge_error = Some(error),
                }
            }
        }
    }

    fn save_answer(&self, index: usize, answer: &str) -> Option<String> {
        let relative = format!("answers/{index}.txt");
        state::create_private_dir_all(&self.dir.join("answers")).ok()?;
        state::write_private(&self.dir.join(&relative), answer).ok()?;
        Some(relative)
    }

    /// Every run starts from the same pristine fixture at the same path.
    fn reset_workdir(&self) -> Result<(), String> {
        match std::fs::remove_dir_all(self.work) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not reset the workdir: {error}")),
        }
        std::fs::create_dir_all(self.work)
            .map_err(|e| format!("could not create the workdir: {e}"))?;
        corpus::materialize(self.work).map_err(|e| format!("could not write the fixture: {e}"))?;
        if self.git_present {
            init_git(self.work)?;
        }
        Ok(())
    }
}

/// Workers and orchestrators alternate (corpus order within each role), so a spend cap that
/// hits mid-run leaves both roles with results.
fn interleave_roles(tasks: &[Task]) -> Vec<&Task> {
    let (workers, orchestrators): (Vec<&Task>, Vec<&Task>) =
        tasks.iter().partition(|task| task.role == Role::Worker);
    let mut workers = workers.into_iter();
    let mut orchestrators = orchestrators.into_iter();
    let mut out = Vec::with_capacity(tasks.len());
    loop {
        let before = out.len();
        out.extend(workers.next());
        out.extend(orchestrators.next());
        if out.len() == before {
            return out;
        }
    }
}

fn base_row(candidate: &Candidate, task: &Task, rep: u32, status: Status) -> Row {
    Row::new(
        &candidate.harness,
        &candidate.model,
        candidate.family.as_deref(),
        &task.id,
        task.role,
        rep,
        status,
    )
}

fn failed_to_start(mut row: Row, error: String) -> Row {
    row.status = Status::Failed;
    row.correctness = Some(0.0);
    row.error = Some(error);
    row
}

fn git(work: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=zirv-benchmark",
            "-c",
            "user.email=benchmark@zirv.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(work)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A committed fixture, so `git diff` later shows exactly what the agent changed.
fn init_git(work: &Path) -> Result<(), String> {
    git(work, &["init", "-q"])?;
    std::fs::write(work.join(".git/info/exclude"), "__pycache__/\n*.pyc\n")
        .map_err(|e| format!("git exclude: {e}"))?;
    git(work, &["add", "-A"])?;
    git(work, &["commit", "-q", "-m", "fixture"])?;
    Ok(())
}

fn git_diff(work: &Path) -> String {
    let staged = git(work, &["add", "-A"]).and_then(|_| git(work, &["diff", "--cached", "HEAD"]));
    truncate_diff(&staged.unwrap_or_default())
}

fn truncate_diff(diff: &str) -> String {
    if diff.len() <= DIFF_LIMIT {
        return diff.to_string();
    }
    let mut end = DIFF_LIMIT;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[diff truncated at {DIFF_LIMIT} bytes]\n", &diff[..end])
}

fn judge_prompt(task_prompt: &str, answer: &str, diff: &str) -> String {
    let diff = if diff.trim().is_empty() {
        "(no changes)"
    } else {
        diff
    };
    format!(
        "You are grading the work of a coding agent. You do not know which agent or model produced it. \
Judge only the evidence below.\n\n\
<task>\n{task_prompt}\n</task>\n\n\
<final_answer>\n{answer}\n</final_answer>\n\n\
<repository_changes>\n{diff}\n</repository_changes>\n\n\
Score the work from 0 (wrong or useless) to 10 (correct, complete and minimal). \
Reply with exactly one JSON object and nothing else: {{\"score\": <integer 0-10>, \"reason\": \"<one sentence>\"}}"
    )
}

fn parse_judge_score(reply: &str) -> Result<f64, String> {
    let candidate = crate::commands::ctx::result_schema::extract_json_candidate(reply)
        .ok_or("judge reply had no JSON object")?;
    let value: serde_json::Value =
        serde_json::from_str(&candidate).map_err(|e| format!("judge reply is not JSON: {e}"))?;
    let score = value
        .get("score")
        .and_then(serde_json::Value::as_f64)
        .ok_or("judge reply has no numeric score")?;
    if !(0.0..=10.0).contains(&score) {
        return Err(format!("judge score {score} is outside 0-10"));
    }
    Ok(score)
}

/// Drop zirv's own health-marker lines so the answer carries nothing a judge could learn from.
pub fn strip_markers(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("[zirv]"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The environment handed to the supervised child: no inherited supervision identity, no extra zirv model calls.
pub fn child_env(base: EnvLookup<'_>, key: &str) -> Option<String> {
    if SUPERVISION_ENV.contains(&key) {
        return None;
    }
    match CHILD_KNOBS.iter().find(|(knob, _)| *knob == key) {
        Some((_, value)) => Some((*value).to_string()),
        None => base(key),
    }
}

fn append_row(dir: &Path, row: &Row) -> Result<(), String> {
    let line = serde_json::to_string(row).map_err(|e| e.to_string())?;
    let mut file = state::open_private_append(&dir.join("results.jsonl"))
        .map_err(|e| format!("results.jsonl: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("results.jsonl: {e}"))
}

/// Create the run directory and `run.json` before the first run starts.
pub fn start_store(dir: &Path, meta: &RunMeta) -> Result<(), String> {
    state::create_private_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    state::write_private(&dir.join("run.json"), &json).map_err(|e| format!("run.json: {e}"))
}

pub fn load_store(dir: &Path) -> Result<(RunMeta, Vec<Row>), String> {
    let meta = std::fs::read_to_string(dir.join("run.json"))
        .map_err(|e| format!("{}: {e}", dir.join("run.json").display()))?;
    let meta: RunMeta = serde_json::from_str(&meta).map_err(|e| format!("run.json: {e}"))?;
    // A crashed run may end on a partial line; every complete row still counts.
    let rows = std::fs::read_to_string(dir.join("results.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    Ok((meta, rows))
}

/// The run directory for `id`, or the newest one (ids sort by start time).
pub fn find_run(root: &Path, id: Option<&str>) -> Result<PathBuf, String> {
    if let Some(id) = id {
        // Generated ids are ASCII alphanumerics only; anything else could leave the root.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(format!("invalid run id '{id}'"));
        }
        let dir = root.join(id);
        return if dir.join("run.json").is_file() {
            Ok(dir)
        } else {
            Err(format!("no benchmark run '{id}'"))
        };
    }
    let mut runs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|_| "no benchmark runs recorded yet".to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("run.json").is_file())
        .collect();
    runs.sort();
    runs.pop()
        .ok_or_else(|| "no benchmark runs recorded yet".to_string())
}

/// Launches real harnesses through the supervised exec path.
pub struct ExecLauncher<'a> {
    pub cfg: &'a CtxConfig,
    pub env: EnvLookup<'a>,
    pub work: &'a Path,
    pub table: &'a PriceTable,
    pub timeout_secs: u64,
}

impl ExecLauncher<'_> {
    pub fn launch(&self, spec: &LaunchSpec) -> Result<Launch, String> {
        let adapter =
            adapters::select(Some(spec.harness), &[], self.cfg).map_err(|e| e.to_string())?;
        let model_flags = if spec.model == DEFAULT_MODEL {
            Vec::new()
        } else {
            adapter.model_args(spec.model)
        };
        let mut command =
            agent::worker_launch_flags(self.cfg, spec.harness, adapter.as_ref(), &model_flags);
        if spec.read_only {
            adapters::extend_read_only_args(
                adapter.as_ref(),
                &mut command,
                adapters::LaunchMode::Headless,
            );
        }
        let args = ExecArgs {
            agent: Some(spec.harness.to_string()),
            prompt: Some(spec.prompt.clone()),
            timeout_secs: Some(self.timeout_secs),
            command,
            ..Default::default()
        };
        let child = |key: &str| child_env(self.env, key);
        let started = Instant::now();
        let (exit_code, report) = exec::run_with_report(&args, &mut Vec::new(), self.work, &child)
            .map_err(|e| e.to_string())?;
        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let usage = report
            .segments
            .iter()
            .fold(TranscriptUsage::default(), |sum, segment| {
                exec::add_usage(&sum, &segment.usage)
            });
        let fallback = Some(spec.model).filter(|m| *m != DEFAULT_MODEL);
        let cost_micros = segments_cost(
            report
                .segments
                .iter()
                .map(|segment| (segment.model.as_deref().or(fallback), &segment.usage)),
            self.table,
        );
        let model = report.segments.last().and_then(|s| s.model.clone());
        let answer = report
            .segments
            .last()
            .and_then(|segment| {
                let session = SessionRef {
                    id: SessionId::parse(&segment.session),
                    cwd: self.work.to_path_buf(),
                };
                let transcript = std::fs::read_to_string(adapter.transcript_path(&session)).ok()?;
                adapter.final_assistant_message(&transcript)
            })
            .map(|text| strip_markers(&text))
            .unwrap_or_default();
        Ok(Launch {
            exit_code,
            wall_ms,
            usage,
            cost_micros,
            model,
            answer,
        })
    }
}

/// Cost is known only when every segment parsed non-zero usage and its model is priced;
/// a zero-usage segment means the harness reported no tokens, never a free run.
fn segments_cost<'a>(
    segments: impl Iterator<Item = (Option<&'a str>, &'a TranscriptUsage)>,
    table: &PriceTable,
) -> Option<u64> {
    let mut total: Option<u64> = None;
    for (model, usage) in segments {
        if *usage == TranscriptUsage::default() {
            return None;
        }
        let cost = price::price(model?, usage, table)?;
        total = Some(total.unwrap_or(0).saturating_add(cost));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::testenv;
    use std::collections::HashMap;

    fn candidate(harness: &str, model: &str) -> Candidate {
        serde_json::from_value(serde_json::json!({
            "harness": harness, "model": model, "family": null
        }))
        .unwrap()
    }

    fn plan_for(candidates: Vec<Candidate>, task_ids: &[&str], reps: u32, max_usd: f64) -> Plan {
        let corpus = corpus::embedded().unwrap();
        let tasks: Vec<Task> = corpus
            .tasks
            .into_iter()
            .filter(|task| task_ids.contains(&task.id.as_str()))
            .collect();
        Plan {
            harnesses: Vec::new(),
            task_ids: tasks.iter().map(|t| t.id.clone()).collect(),
            agent_runs: candidates.len() * tasks.len() * reps as usize,
            judge_calls: 0,
            judge_unpriced: false,
            candidates,
            skipped_tasks: Vec::new(),
            judge: None,
            reps,
            max_usd,
            prices_as_of: "2026-10-01".to_string(),
            python3: true,
            tasks,
        }
    }

    fn launched(cost: Option<u64>) -> Launch {
        Launch {
            exit_code: 0,
            wall_ms: 10,
            usage: TranscriptUsage::default(),
            cost_micros: cost,
            model: None,
            answer: "reserve InsufficientStock".to_string(),
        }
    }

    fn stub_run(
        plan: &Plan,
        launch: &mut dyn FnMut(&LaunchSpec) -> Result<Launch, String>,
    ) -> (tempfile::TempDir, Vec<Row>) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let dir = tmp.path().join("run");
        start_store(&dir, &plan.meta("id", "now")).unwrap();
        let mut runner = Runner {
            dir: &dir,
            work: &work,
            python_present: false,
            git_present: false,
            launch,
            progress: &mut |_| {},
        };
        let rows = runner.execute(plan).unwrap();
        (tmp, rows)
    }

    #[test]
    fn once_spent_reaches_the_cap_remaining_runs_are_skipped_with_a_reason() {
        let plan = plan_for(
            vec![candidate("claude", "m")],
            &["w-read", "o-triage", "o-review"],
            1,
            1.0,
        );
        let mut calls = 0;
        let (_tmp, rows) = stub_run(&plan, &mut |_| {
            calls += 1;
            Ok(launched(Some(1_000_000)))
        });
        assert_eq!(calls, 1);
        assert_eq!(rows[0].status, Status::Ok);
        for skipped in &rows[1..] {
            assert_eq!(skipped.status, Status::Skipped);
            assert_eq!(skipped.skip_reason.as_deref(), Some("spend cap"));
            assert_eq!(skipped.wall_ms, None);
        }
    }

    #[test]
    fn judge_cost_counts_toward_the_cap_but_not_the_candidates_cost() {
        let mut plan = plan_for(
            vec![candidate("claude", "m")],
            &["w-read", "o-triage"],
            1,
            1.0,
        );
        plan.judge = Some(Judge {
            harness: "claude".to_string(),
            model: "j".to_string(),
            also_candidate: false,
        });
        let (_tmp, rows) = stub_run(&plan, &mut |spec| {
            let mut out = launched(Some(if spec.read_only { 1_000_000 } else { 100 }));
            if spec.read_only {
                out.answer = "{\"score\": 7, \"reason\": \"fine\"}".to_string();
            }
            Ok(out)
        });
        assert_eq!(rows[0].cost_micros, Some(100));
        assert_eq!(rows[0].judge_cost_micros, Some(1_000_000));
        assert_eq!(rows[0].judge_score, Some(7.0));
        assert_eq!(rows[1].skip_reason.as_deref(), Some("spend cap"));
    }

    #[test]
    fn a_non_zero_exit_is_a_failed_run_with_zero_correctness() {
        let plan = plan_for(vec![candidate("claude", "m")], &["w-read"], 1, 10.0);
        let (_tmp, rows) = stub_run(&plan, &mut |_| {
            let mut out = launched(Some(5));
            out.exit_code = 76;
            Ok(out)
        });
        assert_eq!(rows[0].status, Status::Failed);
        assert_eq!(rows[0].correctness, Some(0.0));
        assert_eq!(rows[0].exit_code, Some(76));
    }

    #[test]
    fn a_launch_error_is_a_failed_run_not_a_skip() {
        let plan = plan_for(vec![candidate("claude", "m")], &["w-read"], 1, 10.0);
        let (_tmp, rows) = stub_run(&plan, &mut |_| Err("no such harness".to_string()));
        assert_eq!(rows[0].status, Status::Failed);
        assert_eq!(rows[0].error.as_deref(), Some("no such harness"));
    }

    #[test]
    fn graders_score_the_answer_and_an_unparseable_judge_reply_is_recorded() {
        let mut plan = plan_for(vec![candidate("claude", "m")], &["w-read"], 1, 10.0);
        plan.judge = Some(Judge {
            harness: "claude".to_string(),
            model: "j".to_string(),
            also_candidate: false,
        });
        let (_tmp, rows) = stub_run(&plan, &mut |spec| {
            let mut out = launched(None);
            if spec.read_only {
                out.answer = "looks fine to me".to_string();
            }
            Ok(out)
        });
        assert_eq!(rows[0].graders_passed, 2);
        assert_eq!(rows[0].correctness, Some(1.0));
        assert_eq!(rows[0].judge_score, None);
        assert!(rows[0].judge_error.is_some());
    }

    #[test]
    fn the_judge_prompt_is_blind_and_the_prompt_carries_the_notice() {
        let plan = plan_for(vec![candidate("claude", "m")], &["w-read"], 1, 10.0);
        let mut prompts = Vec::new();
        let (_tmp, _rows) = stub_run(&plan, &mut |spec| {
            prompts.push(spec.prompt.clone());
            Ok(launched(None))
        });
        assert!(prompts[0].starts_with("You are running non-interactively"));
        let judge = judge_prompt("do x", "answer", "");
        assert!(judge.contains("(no changes)"));
        assert!(!judge.contains("claude"));
    }

    #[test]
    fn judge_scores_must_be_numbers_in_range() {
        assert_eq!(
            parse_judge_score("{\"score\": 8, \"reason\": \"x\"}"),
            Ok(8.0)
        );
        assert_eq!(parse_judge_score("```json\n{\"score\": 0}\n```"), Ok(0.0));
        assert!(parse_judge_score("{\"score\": 11}").is_err());
        assert!(parse_judge_score("{\"score\": \"high\"}").is_err());
        assert!(parse_judge_score("no json").is_err());
    }

    #[test]
    fn diffs_are_truncated_with_a_note_on_a_char_boundary() {
        assert_eq!(truncate_diff("small"), "small");
        let big = "é".repeat(DIFF_LIMIT);
        let cut = truncate_diff(&big);
        assert!(cut.contains("[diff truncated"));
        assert!(cut.len() < big.len());
    }

    #[test]
    fn health_marker_lines_are_stripped_from_answers() {
        assert_eq!(
            strip_markers("[zirv] step 1\nreal answer\n  [zirv] step 2"),
            "real answer"
        );
    }

    #[test]
    fn run_ids_are_utc_timestamps_with_a_random_suffix() {
        let started = chrono::DateTime::parse_from_rfc3339("2026-10-08T12:34:56Z")
            .unwrap()
            .to_utc();
        let id = run_id(started);
        assert!(id.starts_with("20261008T123456Z"));
        assert_eq!(id.len(), "20261008T123456Z".len() + 4);
        assert_ne!(id, run_id(started));
    }

    #[test]
    fn plan_counts_runs_and_skips_python_only_tasks_without_python() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CtxConfig::load(tmp.path(), &|_| None).unwrap();
        let present = adapters::only_installed(&["claude"]);
        let filters = Filters {
            tasks: vec!["w-bugfix".to_string(), "w-read".to_string()],
            no_judge: true,
            ..Filters::default()
        };
        let with_python =
            plan(&cfg, &filters, 2, 10.0, true, &present, &Listing::default()).unwrap();
        assert_eq!(with_python.agent_runs, 4);
        assert_eq!(with_python.judge_calls, 0);
        let without = plan(
            &cfg,
            &filters,
            2,
            10.0,
            false,
            &present,
            &Listing::default(),
        )
        .unwrap();
        assert_eq!(without.agent_runs, 2);
        assert_eq!(without.skipped_tasks[0].id, "w-bugfix");
        assert!(render_plan(&without).contains("skipped: python3 not found"));
        assert!(
            plan(
                &cfg,
                &Filters {
                    tasks: vec!["nope".to_string()],
                    ..Filters::default()
                },
                1,
                1.0,
                true,
                &present,
                &Listing::default()
            )
            .is_err()
        );
    }

    #[test]
    fn the_plan_states_no_estimate_and_flags_unknown_cost() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CtxConfig::load(tmp.path(), &|_| None).unwrap();
        let present = adapters::only_installed(&["goose"]);
        let text = render_plan(
            &plan(
                &cfg,
                &Filters::default(),
                1,
                10.0,
                true,
                &present,
                &Listing::default(),
            )
            .unwrap(),
        );
        assert!(text.contains("source: models discovered on this machine, see `zirv ctx models`"));
        assert!(text.contains("transcript tokens x the price table"));
        assert!(text.contains("runs with unknown cost are not counted against the spend cap"));
        assert!(!text.contains("cost measured"));
        assert!(text.contains("spend cap: $10.00"));
        assert!(!text.to_lowercase().contains("estimate"));
    }

    #[test]
    fn roles_alternate_so_a_spend_cap_leaves_both_roles_with_results() {
        let ids = [
            "w-read", "w-rename", "w-bugfix", "o-review", "o-plan", "o-triage",
        ];
        let plan = plan_for(vec![candidate("claude", "vendor-model-2")], &ids, 1, 2.0);
        let order: Vec<&str> = interleave_roles(&plan.tasks)
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "w-read", "o-review", "w-rename", "o-plan", "w-bugfix", "o-triage"
            ]
        );
        let (_tmp, rows) = stub_run(&plan, &mut |_| Ok(launched(Some(1_000_000))));
        let ran: Vec<Role> = rows
            .iter()
            .filter(|r| r.status == Status::Ok)
            .map(|r| r.role)
            .collect();
        assert_eq!(ran, vec![Role::Worker, Role::Orchestrator]);
    }

    #[test]
    fn a_missing_default_judge_is_explained_only_when_it_was_not_asked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CtxConfig::load(tmp.path(), &|_| None).unwrap();
        let present = adapters::only_installed(&["claude"]);
        let unpriced = plan(
            &cfg,
            &Filters::default(),
            1,
            10.0,
            true,
            &present,
            &Listing::default(),
        )
        .unwrap();
        assert!(render_plan(&unpriced).contains(
            "judge: none (no candidate has a known output price; pass --judge <harness:model>)"
        ));
        let nobody = plan(
            &cfg,
            &Filters::default(),
            1,
            10.0,
            true,
            &adapters::only_installed(&[]),
            &Listing::default(),
        )
        .unwrap();
        assert!(nobody.candidates.is_empty() && !nobody.judge_unpriced);
        let filters = Filters {
            no_judge: true,
            ..Filters::default()
        };
        let asked = plan(&cfg, &filters, 1, 10.0, true, &present, &Listing::default()).unwrap();
        let text = render_plan(&asked);
        assert!(text.contains("judge: none\n"));
        assert!(!text.contains("known output price"));
    }

    fn test_table() -> PriceTable {
        PriceTable {
            as_of: "2026-01-01".to_string(),
            models: [(
                "vendor-model-2".to_string(),
                price::ModelPrice {
                    input_micros: 1_000_000,
                    cache_write_micros: 1_000_000,
                    cache_read_micros: 100_000,
                    output_micros: 5_000_000,
                },
            )]
            .into(),
        }
    }

    fn usage(input: u64, cache_read: u64, output: u64) -> TranscriptUsage {
        TranscriptUsage {
            input_tokens: input,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cache_read,
            output_tokens: output,
        }
    }

    /// The fake codex agent writes no rollout (so no `token_count`); the pricing rule is tested on a
    /// codex-shaped segment instead: input split from cached input, no cache-write class.
    #[test]
    fn a_codex_shaped_segment_with_tokens_and_a_priced_model_has_a_known_cost() {
        let table = test_table();
        let tokens = usage(1_000, 5_000, 200);
        let cost = segments_cost([(Some("vendor-model-2"), &tokens)].into_iter(), &table);
        assert!(cost.is_some_and(|micros| micros > 0));
    }

    #[test]
    fn a_zero_usage_segment_is_an_unknown_cost_never_free() {
        let table = test_table();
        let none = TranscriptUsage::default();
        assert_eq!(
            segments_cost([(Some("vendor-model-2"), &none)].into_iter(), &table),
            None
        );
        let tokens = usage(10, 0, 10);
        let mixed = [
            (Some("vendor-model-2"), &tokens),
            (Some("vendor-model-2"), &none),
        ];
        assert_eq!(segments_cost(mixed.into_iter(), &table), None);
    }

    #[test]
    fn an_unpriced_or_unnamed_model_is_an_unknown_cost() {
        let table = test_table();
        let tokens = usage(10, 0, 10);
        assert_eq!(
            segments_cost([(Some("no-such-model"), &tokens)].into_iter(), &table),
            None
        );
        assert_eq!(segments_cost([(None, &tokens)].into_iter(), &table), None);
        assert_eq!(segments_cost(std::iter::empty(), &table), None);
    }

    #[test]
    fn segment_costs_add_up() {
        let table = test_table();
        let tokens = usage(1_000, 0, 100);
        let one = segments_cost([(Some("vendor-model-2"), &tokens)].into_iter(), &table).unwrap();
        let two = segments_cost(
            [
                (Some("vendor-model-2"), &tokens),
                (Some("vendor-model-2"), &tokens),
            ]
            .into_iter(),
            &table,
        );
        assert_eq!(two, Some(one * 2));
    }

    #[test]
    fn the_row_records_the_model_the_transcript_names_not_the_default_label() {
        let plan = plan_for(
            vec![candidate("claude", DEFAULT_MODEL)],
            &["w-read"],
            1,
            10.0,
        );
        let (_tmp, rows) = stub_run(&plan, &mut |_| {
            let mut out = launched(None);
            out.model = Some("vendor-model-2".to_string());
            Ok(out)
        });
        assert_eq!(rows[0].model, "vendor-model-2");
    }

    #[test]
    fn a_stored_run_reloads_and_tolerates_a_torn_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("20260101T000000Z0000");
        let meta = RunMeta::for_test();
        start_store(&dir, &meta).unwrap();
        let row = Row::new("claude", "m", None, "w-read", Role::Worker, 0, Status::Ok);
        append_row(&dir, &row).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("results.jsonl"))
            .unwrap()
            .write_all(b"{\"schema\": 1, \"harn")
            .unwrap();
        let (loaded, rows) = load_store(&dir).unwrap();
        assert_eq!(loaded.run_id, meta.run_id);
        assert_eq!(rows, vec![row]);
        assert_eq!(find_run(tmp.path(), None).unwrap(), dir);
        assert_eq!(
            find_run(tmp.path(), Some("20260101T000000Z0000")).unwrap(),
            dir
        );
        assert!(find_run(tmp.path(), Some("missing")).is_err());
        let outside = tmp.path().parent().unwrap().join("x");
        for bad in ["../x", "..", "a/b", "", outside.to_str().unwrap()] {
            assert_eq!(
                find_run(tmp.path(), Some(bad)),
                Err(format!("invalid run id '{bad}'"))
            );
        }
    }

    #[test]
    fn the_child_env_scrubs_supervision_identity_and_forces_the_knobs() {
        let base: HashMap<&str, &str> = [
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::SESSION_ENV, "outer"),
            ("ZIRV_CTX_PACE", "true"),
            ("KEEP", "yes"),
        ]
        .into();
        let lookup = |key: &str| base.get(key).map(|v| (*v).to_string());
        assert_eq!(child_env(&lookup, adapters::SEAT_ROLE_ENV), None);
        assert_eq!(child_env(&lookup, adapters::SESSION_ENV), None);
        assert_eq!(
            child_env(&lookup, "ZIRV_CTX_PACE").as_deref(),
            Some("false")
        );
        assert_eq!(
            child_env(&lookup, "ZIRV_CTX_SUPERVISOR_ENABLED").as_deref(),
            Some("false")
        );
        assert_eq!(
            child_env(&lookup, "ZIRV_CTX_MEMORY_HARVEST").as_deref(),
            Some("false")
        );
        assert_eq!(child_env(&lookup, "KEEP").as_deref(), Some("yes"));
    }

    #[test]
    fn git_fixtures_show_the_agents_changes_as_a_diff() {
        if !adapters::program_is_present("git") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let dir = tmp.path().join("run");
        let mut launch = |_: &LaunchSpec| Ok(launched(None));
        let runner = Runner {
            dir: &dir,
            work: &work,
            python_present: false,
            git_present: true,
            launch: &mut launch,
            progress: &mut |_| {},
        };
        runner.reset_workdir().unwrap();
        std::fs::write(work.join("tinyshop/new.py"), "X = 1\n").unwrap();
        std::fs::write(work.join("README.md"), "changed\n").unwrap();
        let diff = git_diff(&work);
        assert!(diff.contains("tinyshop/new.py"));
        assert!(diff.contains("+changed"));
        assert!(!diff.contains("pycache"));
        runner.reset_workdir().unwrap();
        assert!(!work.join("tinyshop/new.py").exists());
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// Env for a real supervised child that is the fake agent, behind an optional wrapper command.
    fn fake_env(state: &Path, bin: String) -> HashMap<String, String> {
        [
            (state::STATE_ENV.to_string(), state.display().to_string()),
            ("ZIRV_CTX_AGENT_BIN".to_string(), bin),
            (
                "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
                "0".to_string(),
            ),
            (
                "ZIRV_CTX_PROMPT_SKILL_INDEX".to_string(),
                "false".to_string(),
            ),
        ]
        .into()
    }

    #[test]
    fn an_end_to_end_run_records_a_row_and_removes_the_workdir() {
        let tmp = testenv::repo();
        let _home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let env = fake_env(
            &tmp.path().join("state"),
            format!("sh {}", fixture("fake-agent.sh").display()),
        );
        let lookup = |key: &str| env.get(key).cloned();
        let cfg = CtxConfig::load(tmp.path(), &lookup).unwrap();
        let table = price::resolve_table(&cfg);
        let launcher = ExecLauncher {
            cfg: &cfg,
            env: &lookup,
            work: &tmp.path().join("work"),
            table: &table,
            timeout_secs: 60,
        };
        let plan = plan_for(
            vec![candidate("claude", DEFAULT_MODEL)],
            &["w-read"],
            1,
            10.0,
        );
        let dir = tmp.path().join("run");
        start_store(&dir, &plan.meta("id", "now")).unwrap();
        let work = tmp.path().join("work");
        let mut launch = |spec: &LaunchSpec| launcher.launch(spec);
        let mut runner = Runner {
            dir: &dir,
            work: &work,
            python_present: false,
            git_present: adapters::program_is_present("git"),
            launch: &mut launch,
            progress: &mut |_| {},
        };
        // SAFETY: CI runs tests single-threaded.
        unsafe { std::env::set_var("FAKE_AGENT_MODE", "healthy") };
        let rows = runner.execute(&plan);
        unsafe { std::env::remove_var("FAKE_AGENT_MODE") };
        let rows = rows.unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Ok);
        assert_eq!(rows[0].exit_code, Some(0));
        assert!(rows[0].wall_ms.is_some());
        assert!(rows[0].input_tokens + rows[0].cache_read_input_tokens > 0);
        assert_ne!(
            rows[0].model, DEFAULT_MODEL,
            "the row names the model actually used"
        );
        assert!(
            rows[0].cost_micros.is_some(),
            "tokens plus a priced model give a cost"
        );
        let answer = dir.join(rows[0].answer_file.as_deref().expect("answer file"));
        assert!(answer.is_file());
        let (_, stored) = load_store(&dir).unwrap();
        assert_eq!(stored, rows);
        assert!(!work.exists(), "the workdir is removed afterwards");
    }

    #[test]
    fn a_child_started_inside_a_supervised_session_is_not_refused_and_loses_the_seat_role() {
        let tmp = testenv::repo();
        let _home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let log = tmp.path().join("child-env.log");
        let wrapper = tmp.path().join("wrap.sh");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s\\n' \"${{{seat}:-unset}}\" \"${{{session}:-unset}}\" \"${{ZIRV_CTX_PACE:-unset}}\" >> '{log}'\nexec sh '{agent}' \"$@\"\n",
                seat = adapters::SEAT_ROLE_ENV,
                session = adapters::SESSION_ENV,
                log = log.display(),
                agent = fixture("fake-agent.sh").display()
            ),
        )
        .unwrap();
        let mut env = fake_env(
            &tmp.path().join("state"),
            format!("sh {}", wrapper.display()),
        );
        env.insert(
            adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        );
        env.insert(
            adapters::SESSION_ENV.to_string(),
            "outer-session".to_string(),
        );
        let lookup = |key: &str| env.get(key).cloned();
        let cfg = CtxConfig::load(tmp.path(), &lookup).unwrap();
        let table = price::resolve_table(&cfg);
        let launcher = ExecLauncher {
            cfg: &cfg,
            env: &lookup,
            work: &tmp.path().join("work"),
            table: &table,
            timeout_secs: 60,
        };
        std::fs::create_dir_all(tmp.path().join("work")).unwrap();
        // SAFETY: CI runs tests single-threaded; the real process env is what a child would inherit.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var(adapters::SEAT_ROLE_ENV, "orchestrator");
            std::env::set_var(adapters::SESSION_ENV, "outer-session");
            std::env::set_var("ZIRV_CTX_PACE", "true");
        }
        let spec = LaunchSpec {
            harness: "claude",
            model: DEFAULT_MODEL,
            prompt: "p".to_string(),
            read_only: false,
        };
        let launched = launcher.launch(&spec);
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var(adapters::SEAT_ROLE_ENV);
            std::env::remove_var(adapters::SESSION_ENV);
            std::env::remove_var("ZIRV_CTX_PACE");
        }
        assert_eq!(launched.expect("not refused").exit_code, 0);
        let seen = std::fs::read_to_string(&log).unwrap();
        // Earlier lines are zirv's own capability probes of the binary; the last is the real launch.
        let launch = seen.lines().last().expect("the child ran");
        let fields: Vec<&str> = launch.split('|').collect();
        assert_ne!(
            fields[0], "orchestrator",
            "the seat role must not leak: {seen}"
        );
        assert_ne!(
            fields[1], "outer-session",
            "the outer session must not leak: {seen}"
        );
    }
}
