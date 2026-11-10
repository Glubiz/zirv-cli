//! `zirv benchmark eval`: behaviour cases that grade what an agent does (its tool calls), not
//! what it says. `lint`, `plan`, `report` and `run --dry-run` are free and never launch a harness;
//! `run --yes` replays the cases through a real headless harness and spends model quota.

mod corpus;
mod grade;
mod replay;
mod trace;

use std::collections::{BTreeMap, btree_map::Entry};
use std::io::Write;
use std::path::Path;

use clap::Subcommand;
use serde_json::json;

use super::{DEFAULT_MAX_USD, DEFAULT_TIMEOUT_SECS, parse_usd};
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::price;
use crate::commands::ctx::state::{self, StateDir};

const DEFAULT_SAMPLES: u32 = 2;

#[derive(Debug, Subcommand)]
pub(super) enum EvalCommand {
    /// Validate the corpus: surfaces resolve, bad cases cite a source, canned traces grade as expected.
    Lint {
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// List the cases, without any model call.
    Plan {
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Replay the cases through a real headless harness with every tool call denied, and grade
    /// the recorded calls. `--dry-run` spends nothing; without it, `--yes` is required.
    Run {
        /// Print the resolved surface text, turns and bundle hash; launch nothing.
        #[arg(long, conflicts_with_all = ["yes", "baseline", "stop_on_regression"])]
        dry_run: bool,
        /// Also print the per-surface size.
        #[arg(long)]
        context: bool,
        /// Restrict to these case ids.
        #[arg(long = "case", value_name = "id")]
        cases: Vec<String>,
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
        /// Samples per case; a tie on an even count adds one more. A case fails when at least half fail.
        #[arg(long, default_value_t = DEFAULT_SAMPLES, value_parser = clap::value_parser!(u32).range(1..))]
        samples: u32,
        /// Stop launching new samples once measured spend reaches this many USD.
        #[arg(long, default_value_t = DEFAULT_MAX_USD, value_parser = parse_usd)]
        max_usd: f64,
        /// Wall-clock limit for one harness run.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
        timeout_secs: u64,
        /// Compare against this earlier eval run.
        #[arg(long, value_name = "run-id")]
        baseline: Option<String>,
        /// Stop after the first case that passed in the baseline and fails now.
        #[arg(long, requires = "baseline")]
        stop_on_regression: bool,
        /// Confirm that this spends real model quota.
        #[arg(long)]
        yes: bool,
    },
    /// Re-render a stored eval run (the newest when no id is given), with its baseline comparison.
    Report {
        run_id: Option<String>,
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
    },
}

pub(super) fn execute(
    command: EvalCommand,
    repo: &Path,
    env: EnvLookup<'_>,
    out: &mut dyn Write,
) -> Result<i32, String> {
    execute_with(command, repo, env, out, None)
}

/// An injected launcher replaces the real harness launch (tests only).
fn execute_with(
    command: EvalCommand,
    repo: &Path,
    env: EnvLookup<'_>,
    out: &mut dyn Write,
    injected: Option<&mut replay::Launcher<'_>>,
) -> Result<i32, String> {
    let cases = corpus::embedded()?;
    match command {
        EvalCommand::Report { run_id, json } => {
            replay::report(&eval_root(env)?, run_id.as_deref(), json, out)
        }
        EvalCommand::Lint { json } => lint(&cases, repo, env, out, json),
        EvalCommand::Plan { json } => {
            let rows: Vec<_> = cases
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id, "kind": c.kind, "surface": c.surface, "role": c.role,
                        "harness": c.harness(), "turns": c.turns.len(),
                    })
                })
                .collect();
            let text: String = cases
                .iter()
                .map(|c| {
                    format!(
                        "{:<36} {:<8} {:<34} {}\n",
                        c.id,
                        format!("{:?}", c.kind).to_lowercase(),
                        c.surface,
                        c.role
                    )
                })
                .collect();
            super::emit(out, json, &rows, &text)?;
            Ok(0)
        }
        EvalCommand::Run {
            dry_run,
            context,
            cases: wanted,
            json,
            samples,
            max_usd,
            timeout_secs,
            baseline,
            stop_on_regression,
            yes,
        } => {
            if let Some(unknown) = wanted.iter().find(|id| !cases.iter().any(|c| &c.id == *id)) {
                crate::output::error(format!("unknown eval case '{unknown}'"));
                return Ok(2);
            }
            if dry_run {
                return dry_run_report(&cases, &wanted, repo, env, out, context, json);
            }
            let mut selected = Vec::new();
            for case in cases
                .iter()
                .filter(|c| wanted.is_empty() || wanted.contains(&c.id))
            {
                let bundle = corpus::resolve_surface(&case.surface, case.harness(), repo, env)?
                    .bundle_sha256;
                selected.push((case, bundle));
            }
            if !yes {
                let plan = replay::render_plan(&selected, samples, max_usd);
                if json {
                    let rows: Vec<_> = selected
                        .iter()
                        .map(|(c, bundle)| {
                            json!({ "id": c.id, "harness": c.harness(), "bundle_sha256": bundle })
                        })
                        .collect();
                    super::emit(
                        out,
                        true,
                        &json!({ "cases": rows, "samples": samples, "max_usd": max_usd }),
                        &plan,
                    )?;
                    eprintln!("re-run with --yes to start; this spends real model quota");
                } else {
                    super::emit(out, false, &plan, &plan)?;
                    writeln!(
                        out,
                        "\nre-run with --yes to start; this spends real model quota"
                    )
                    .map_err(|e| e.to_string())?;
                }
                return Ok(2);
            }
            let root = eval_root(env)?;
            let work_root = std::fs::canonicalize(std::env::temp_dir())
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("zirv-eval")
                .join(uuid::Uuid::new_v4().simple().to_string());
            state::create_private_dir_all(&work_root)
                .map_err(|e| format!("{}: {e}", work_root.display()))?;
            let work = work_root.join("work");
            let opts = replay::Opts {
                samples,
                max_usd,
                baseline: baseline.as_deref(),
                stop_on_regression,
                json,
                root: &root,
                work: &work,
            };
            let progress = &mut |line: &str| eprintln!("{line}");
            let code = match injected {
                Some(launch) => replay::run(&selected, &opts, launch, progress, out),
                None => {
                    let cfg = CtxConfig::load(repo, env).map_err(|e| e.to_string())?;
                    let table = price::resolve_table(&cfg);
                    let launcher = super::run::ExecLauncher {
                        cfg: &cfg,
                        env,
                        work: &work,
                        table: &table,
                        timeout_secs,
                    };
                    super::run::export_child_knobs();
                    replay::run(
                        &selected,
                        &opts,
                        &mut |spec| launcher.launch(spec),
                        progress,
                        out,
                    )
                }
            };
            let _ = std::fs::remove_dir_all(&work_root);
            code
        }
    }
}

fn eval_root(env: EnvLookup<'_>) -> Result<std::path::PathBuf, String> {
    let state = StateDir::resolve(env).map_err(|e| e.to_string())?;
    Ok(state.root().join("eval"))
}

fn lint(
    cases: &[corpus::Case],
    repo: &Path,
    env: EnvLookup<'_>,
    out: &mut dyn Write,
    json: bool,
) -> Result<i32, String> {
    let problems = corpus::lint(
        cases,
        &|name| corpus::canned_trace(name).map(str::to_string),
        &|surface| corpus::resolve_surface(surface, corpus::DEFAULT_HARNESS, repo, env).map(|_| ()),
    );
    let text = if problems.is_empty() {
        format!("ok: {} cases lint clean\n", cases.len())
    } else {
        problems.iter().map(|p| format!("{p}\n")).collect()
    };
    super::emit(
        out,
        json,
        &json!({ "cases": cases.len(), "problems": problems }),
        &text,
    )?;
    Ok(i32::from(!problems.is_empty()))
}

fn dry_run_report(
    cases: &[corpus::Case],
    wanted: &[String],
    repo: &Path,
    env: EnvLookup<'_>,
    out: &mut dyn Write,
    context: bool,
    json: bool,
) -> Result<i32, String> {
    let mut resolved: BTreeMap<(&str, &str), corpus::Resolved> = BTreeMap::new();
    let mut rows = Vec::new();
    let mut text = String::new();
    for case in cases
        .iter()
        .filter(|c| wanted.is_empty() || wanted.contains(&c.id))
    {
        let key = (case.surface.as_str(), case.harness());
        let surface = match resolved.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(corpus::resolve_surface(
                &case.surface,
                case.harness(),
                repo,
                env,
            )?),
        };
        text.push_str(&format!(
            "== {} ({}) ==\nsurface: {}\nbundle sha256: {}\nrole: {}  harness: {}\nturns:\n",
            case.id,
            format!("{:?}", case.kind).to_lowercase(),
            case.surface,
            surface.bundle_sha256,
            case.role,
            case.harness()
        ));
        for (i, turn) in case.turns.iter().enumerate() {
            text.push_str(&format!("  {}. {turn}\n", i + 1));
        }
        text.push_str(&format!("experience: {}\n", case.experience));
        for prior in &case.setup.prior_turns {
            text.push_str(&format!("prior turn: {prior}\n"));
        }
        for path in case.setup.files.keys() {
            text.push_str(&format!("setup file: {path}\n"));
        }
        text.push_str(&format!("--- surface text ---\n{}\n", surface.text));
        if context {
            text.push_str(&format!("--- context ---\n{}\n", surface.context));
        }
        text.push('\n');
        let mut row = json!({
            "id": case.id, "surface": case.surface, "bundle_sha256": surface.bundle_sha256,
            "role": case.role, "harness": case.harness(), "turns": case.turns,
            "surface_text": surface.text, "experience": case.experience,
            "prior_turns": case.setup.prior_turns, "setup_files": case.setup.files,
        });
        if context {
            row["context"] = json!(surface.context);
        }
        rows.push(row);
    }
    super::emit(out, json, &rows, &text)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::BenchmarkCli;
    use super::*;
    use crate::commands::ctx::state;
    use crate::commands::ctx::testenv;
    use clap::Parser;
    use std::collections::HashMap;

    fn eval_command(args: &[&str]) -> EvalCommand {
        let argv = ["zirv benchmark", "eval"]
            .into_iter()
            .chain(args.iter().copied());
        match BenchmarkCli::try_parse_from(argv).unwrap().command {
            super::super::Command::Eval { command } => command,
            other => panic!("expected eval, got {other:?}"),
        }
    }

    /// Fields drop in order: the working directory is restored before the temp dir goes.
    struct Harness {
        _cwd: testenv::CwdGuard,
        _home: testenv::HomeGuard,
        tmp: testenv::TestRepo,
        argv_log: std::path::PathBuf,
        env: HashMap<&'static str, String>,
    }

    fn harness() -> Harness {
        let tmp = testenv::repo();
        let home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let argv_log = tmp.path().join("argv.log");
        let fake = format!(
            "sh {}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake-agent.sh")
                .display()
        );
        let env: HashMap<&'static str, String> = [
            (
                state::STATE_ENV,
                tmp.path().join("state").display().to_string(),
            ),
            ("ZIRV_CTX_AGENT_BIN", fake),
        ]
        .into();
        // SAFETY: CI runs tests single-threaded.
        unsafe { std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log) };
        let cwd = testenv::CwdGuard::enter(tmp.path()).unwrap();
        Harness {
            tmp,
            _home: home,
            _cwd: cwd,
            argv_log,
            env,
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // SAFETY: CI runs tests single-threaded.
            unsafe { std::env::remove_var("FAKE_AGENT_ARGV_LOG") };
        }
    }

    fn run(h: &Harness, args: &[&str]) -> (Result<i32, String>, String) {
        let lookup = |key: &str| h.env.get(key).cloned();
        let mut out = Vec::new();
        let code = execute(eval_command(args), h.tmp.path(), &lookup, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    /// Cases by sample outcome: `outcome(case, sample)` is whether that sample behaves.
    /// Returns every launch as (case id, spec flags) in order.
    type Launches = std::cell::RefCell<Vec<(String, bool, bool)>>;

    fn replay_with(
        h: &Harness,
        args: &[&str],
        cost: Option<u64>,
        launches: &Launches,
        outcome: &dyn Fn(&str, u32) -> bool,
    ) -> (Result<i32, String>, String) {
        let cases = corpus::embedded().unwrap();
        let mut seen: HashMap<String, u32> = HashMap::new();
        let mut launch = |spec: &super::super::run::LaunchSpec| {
            let case = cases
                .iter()
                .find(|c| spec.prompt.contains(&c.turns[0]))
                .ok_or("no case for this prompt")?;
            let sample = seen.entry(case.id.clone()).or_default();
            let behaves = outcome(&case.id, *sample);
            *sample += 1;
            launches
                .borrow_mut()
                .push((case.id.clone(), spec.deny_all, spec.read_only));
            let canned = match (&case.canned.fail, behaves) {
                (Some(fail), false) => fail,
                _ => &case.canned.pass,
            };
            Ok(super::super::run::Launch {
                exit_code: 0,
                wall_ms: 1,
                usage: Default::default(),
                cost_micros: cost,
                model: None,
                answer: String::new(),
                transcript: corpus::canned_trace(canned).unwrap().to_string(),
            })
        };
        let lookup = |key: &str| h.env.get(key).cloned();
        let mut out = Vec::new();
        let code = execute_with(
            eval_command(args),
            h.tmp.path(),
            &lookup,
            &mut out,
            Some(&mut launch),
        );
        (code, String::from_utf8(out).unwrap())
    }

    fn json_run(
        h: &Harness,
        args: &[&str],
        cost: Option<u64>,
        launches: &Launches,
        outcome: &dyn Fn(&str, u32) -> bool,
    ) -> (i32, serde_json::Value) {
        let mut argv = vec!["run", "--yes", "--json"];
        argv.extend_from_slice(args);
        let (code, out) = replay_with(h, &argv, cost, launches, outcome);
        (
            code.unwrap(),
            serde_json::from_str(&out).unwrap_or_default(),
        )
    }

    fn verdict_of<'a>(report: &'a serde_json::Value, case: &str) -> &'a serde_json::Value {
        report["verdicts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["case"] == case)
            .unwrap_or_else(|| panic!("no verdict for {case} in {report}"))
    }

    const BAD: &str = "skill-load-via-shell";
    const GUARD: &str = "no-unprompted-spend";

    #[test]
    fn run_without_yes_prints_the_plan_exits_2_and_spawns_nothing() {
        let h = harness();
        let launches = Launches::default();
        let (code, out) = replay_with(&h, &["run", "--case", BAD], None, &launches, &|_, _| true);
        assert_eq!(code, Ok(2));
        assert!(out.contains(BAD) && out.contains("--yes"), "{out}");
        assert!(launches.borrow().is_empty(), "no agent may be launched");
        assert!(!h.argv_log.exists());
        assert!(!h.tmp.path().join("state/eval").exists());
        // `--context`, `--json` and a baseline do not make it a paid replay either.
        let (code, _) = replay_with(
            &h,
            &[
                "run",
                "--context",
                "--json",
                "--baseline",
                "x",
                "--case",
                BAD,
            ],
            None,
            &launches,
            &|_, _| true,
        );
        assert_eq!(code, Ok(2));
        assert!(launches.borrow().is_empty());
    }

    #[test]
    fn the_spend_cap_stops_the_run_and_records_the_skipped_samples() {
        let h = harness();
        let launches = Launches::default();
        // $0.0006 a launch against a $0.001 cap: two launches reach it, the rest are skipped.
        let (code, report) = json_run(
            &h,
            &["--case", BAD, "--case", GUARD, "--max-usd", "0.001"],
            Some(600),
            &launches,
            &|_, _| true,
        );
        assert_eq!(code, 0);
        assert_eq!(launches.borrow().len(), 2, "{:?}", launches.borrow());
        assert_eq!(report["skipped_samples"], 2);
        assert_eq!(report["spend_micros"], 1200);
        assert!(report["verdicts"].as_array().unwrap().len() == 1);
        let results = std::fs::read_to_string(
            std::fs::read_dir(h.tmp.path().join("state/eval"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .join("results.jsonl"),
        )
        .unwrap();
        assert!(
            results.contains("\"skip_reason\":\"spend cap\""),
            "{results}"
        );
    }

    #[test]
    fn launches_deny_every_tool_call_and_rows_carry_the_checks_and_bundle_hash() {
        let h = harness();
        let launches = Launches::default();
        let (_, report) = json_run(&h, &["--case", BAD], None, &launches, &|_, _| true);
        assert!(launches.borrow().iter().all(|(_, deny, ro)| *deny && !*ro));
        let dir = std::fs::read_dir(h.tmp.path().join("state/eval"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(dir.join("run.json").is_file());
        assert!(dir.join(format!("traces/{BAD}-0.jsonl")).is_file());
        let row: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(dir.join("results.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(row["case"], BAD);
        assert_eq!(row["harness"], "claude");
        assert_eq!(row["sample"], 0);
        assert_eq!(row["passed"], true);
        assert_eq!(
            row["bundle_sha256"],
            verdict_of(&report, BAD)["bundle_sha256"]
        );
        assert!(!row["checks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_case_fails_when_half_its_samples_fail_and_a_tie_adds_a_third_sample() {
        let h = harness();
        // Both pass: two launches, pass.
        let launches = Launches::default();
        let (_, report) = json_run(&h, &["--case", BAD], None, &launches, &|_, _| true);
        assert_eq!(launches.borrow().len(), 2);
        assert_eq!(verdict_of(&report, BAD)["passed"], true);
        // Both fail: two launches, fail.
        let launches = Launches::default();
        let (_, report) = json_run(&h, &["--case", BAD], None, &launches, &|_, _| false);
        assert_eq!(launches.borrow().len(), 2);
        assert_eq!(verdict_of(&report, BAD)["passed"], false);
        // They disagree: a third sample decides.
        let launches = Launches::default();
        let (_, report) = json_run(&h, &["--case", BAD], None, &launches, &|_, n| n != 0);
        assert_eq!(launches.borrow().len(), 3);
        let verdict = verdict_of(&report, BAD);
        assert_eq!(
            (verdict["samples"].as_u64(), verdict["passed"].as_bool()),
            (Some(3), Some(true))
        );
        let launches = Launches::default();
        let (_, report) = json_run(&h, &["--case", BAD], None, &launches, &|_, n| n == 1);
        assert_eq!(launches.borrow().len(), 3);
        assert_eq!(verdict_of(&report, BAD)["passed"], false);
        // One sample that fails is half or more of one.
        let launches = Launches::default();
        let (_, report) = json_run(
            &h,
            &["--case", BAD, "--samples", "1"],
            None,
            &launches,
            &|_, _| false,
        );
        assert_eq!(launches.borrow().len(), 1);
        assert_eq!(verdict_of(&report, BAD)["passed"], false);
    }

    #[test]
    fn a_case_that_passed_in_the_baseline_and_fails_now_is_a_regression() {
        let h = harness();
        let launches = Launches::default();
        let (_, base) = json_run(
            &h,
            &["--case", BAD, "--case", GUARD],
            None,
            &launches,
            &|_, _| true,
        );
        let base_id = base["run_id"].as_str().unwrap().to_string();
        let (code, now) = json_run(
            &h,
            &["--case", BAD, "--case", GUARD, "--baseline", &base_id],
            None,
            &launches,
            &|case, _| case != BAD,
        );
        assert_eq!(code, 1, "a regression exits 1");
        assert_eq!(now["regressions"], 1);
        let changes: Vec<_> = now["comparisons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["case"].as_str().unwrap(), c["change"].as_str().unwrap()))
            .collect();
        assert_eq!(changes, [(BAD, "regression"), (GUARD, "unchanged")]);
        // An unknown baseline fails before anything is launched.
        let before = launches.borrow().len();
        let (code, _) = replay_with(
            &h,
            &["run", "--yes", "--baseline", "nope", "--case", BAD],
            None,
            &launches,
            &|_, _| true,
        );
        assert!(code.is_err());
        assert_eq!(launches.borrow().len(), before);
    }

    #[test]
    fn stop_on_regression_stops_before_the_next_case() {
        let h = harness();
        let launches = Launches::default();
        let (_, base) = json_run(
            &h,
            &["--case", BAD, "--case", GUARD],
            None,
            &launches,
            &|_, _| true,
        );
        let base_id = base["run_id"].as_str().unwrap().to_string();
        let launches = Launches::default();
        let (code, now) = json_run(
            &h,
            &[
                "--case",
                BAD,
                "--case",
                GUARD,
                "--baseline",
                &base_id,
                "--stop-on-regression",
            ],
            None,
            &launches,
            &|case, _| case != BAD,
        );
        assert_eq!(code, 1);
        assert!(
            launches.borrow().iter().all(|(case, _, _)| case == BAD),
            "{:?}",
            launches.borrow()
        );
        assert_eq!(now["skipped_samples"], 2);
        assert!(now["verdicts"].as_array().unwrap().len() == 1);
        // Without the flag the run goes on.
        let launches = Launches::default();
        json_run(
            &h,
            &["--case", BAD, "--case", GUARD, "--baseline", &base_id],
            None,
            &launches,
            &|case, _| case != BAD,
        );
        assert!(launches.borrow().iter().any(|(case, _, _)| case == GUARD));
    }

    #[test]
    fn a_bundle_hash_mismatch_is_reported_as_a_changed_surface() {
        let h = harness();
        let launches = Launches::default();
        let (_, base) = json_run(&h, &["--case", BAD], None, &launches, &|_, _| true);
        let base_id = base["run_id"].as_str().unwrap();
        let results = h
            .tmp
            .path()
            .join("state/eval")
            .join(base_id)
            .join("results.jsonl");
        let real = base["verdicts"][0]["bundle_sha256"].as_str().unwrap();
        let text = std::fs::read_to_string(&results).unwrap();
        std::fs::write(&results, text.replace(real, &"0".repeat(64))).unwrap();
        let (_, now) = json_run(
            &h,
            &["--case", BAD, "--baseline", base_id],
            None,
            &launches,
            &|_, _| true,
        );
        assert_eq!(now["surface_changed"], 1);
        assert_eq!(now["comparisons"][0]["surface_changed"], true);
        let (_, text) = replay_with(
            &h,
            &["run", "--yes", "--case", BAD, "--baseline", base_id],
            None,
            &launches,
            &|_, _| true,
        );
        assert!(text.contains("surface changed"), "{text}");
        // Same surface: nothing to report.
        let (_, same) = json_run(
            &h,
            &["--case", BAD, "--baseline", now["run_id"].as_str().unwrap()],
            None,
            &launches,
            &|_, _| true,
        );
        assert_eq!(same["surface_changed"], 0);
    }

    #[test]
    fn report_renders_the_newest_run_and_its_json_is_one_document() {
        let h = harness();
        assert!(run(&h, &["report"]).0.is_err(), "no runs yet");
        let launches = Launches::default();
        let (_, first) = json_run(&h, &["--case", BAD], None, &launches, &|_, _| true);
        let first_id = first["run_id"].as_str().unwrap().to_string();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let (_, second) = json_run(
            &h,
            &["--case", BAD, "--baseline", &first_id],
            None,
            &launches,
            &|_, _| false,
        );
        let (code, text) = run(&h, &["report"]);
        assert_eq!(code, Ok(0));
        assert!(text.contains(second["run_id"].as_str().unwrap()), "{text}");
        assert!(
            text.contains("FAIL") && text.contains(BAD) && text.contains("REGRESSION"),
            "{text}"
        );
        let (_, json) = run(&h, &["report", &first_id, "--json"]);
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(doc["run_id"], first_id.as_str());
        assert_eq!(doc["verdicts"][0]["case"], BAD);
        let (_, json) = run(&h, &["report", "--json"]);
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(doc["regressions"], 1);
    }

    #[test]
    fn dry_run_prints_the_surface_turns_and_bundle_hash() {
        let h = harness();
        let (code, out) = run(&h, &["run", "--dry-run", "--case", "no-unprompted-spend"]);
        assert_eq!(code, Ok(0));
        let resolved = corpus::resolve_surface(
            "safety:Bash(zirv benchmark run*)",
            "claude",
            h.tmp.path(),
            &|key: &str| h.env.get(key).cloned(),
        )
        .unwrap();
        assert!(out.contains(&resolved.bundle_sha256), "{out}");
        assert!(out.contains("spends real model quota"), "{out}");
        assert!(out.contains("Which model is best"), "{out}");
        assert!(!out.contains("no-unprompted-spend-explicit"));
        assert!(!h.argv_log.exists());
    }

    #[test]
    fn dry_run_context_prints_the_measure_table_and_json_is_one_document() {
        let h = harness();
        let (_, plain) = run(&h, &["run", "--dry-run", "--case", "branch-before-push"]);
        assert!(!plain.contains("--- context ---"));
        let (code, text) = run(
            &h,
            &[
                "run",
                "--dry-run",
                "--context",
                "--case",
                "branch-before-push",
            ],
        );
        assert_eq!(code, Ok(0));
        assert!(text.contains("total (session prefix)"), "{text}");
        let (_, json) = run(&h, &["run", "--dry-run", "--context", "--json"]);
        let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 10);
        assert!(rows[0]["bundle_sha256"].as_str().unwrap().len() == 64);
        assert!(rows[0]["context"].is_string());
    }

    #[test]
    fn dry_run_cannot_be_combined_with_a_paid_flag_and_a_stop_needs_a_baseline() {
        let parse = |args: &[&str]| {
            BenchmarkCli::try_parse_from(
                ["zirv benchmark", "eval"]
                    .into_iter()
                    .chain(args.iter().copied()),
            )
        };
        assert!(parse(&["run", "--dry-run", "--yes"]).is_err());
        assert!(parse(&["run", "--dry-run", "--baseline", "x"]).is_err());
        assert!(parse(&["run", "--yes", "--stop-on-regression"]).is_err());
        assert!(parse(&["run", "--samples", "0"]).is_err());
        assert!(parse(&["run", "--yes", "--baseline", "x", "--stop-on-regression"]).is_ok());
    }

    #[test]
    fn an_unknown_case_id_exits_2() {
        let h = harness();
        assert_eq!(run(&h, &["run", "--dry-run", "--case", "ghost"]).0, Ok(2));
    }

    #[test]
    fn plan_lists_every_case_and_lint_passes_on_the_seed_corpus() {
        let h = harness();
        let (code, out) = run(&h, &["plan"]);
        assert_eq!(code, Ok(0));
        for id in ["skill-load-via-shell", "recall-trivial-question"] {
            assert!(out.contains(id), "{out}");
        }
        let (code, out) = run(&h, &["lint"]);
        assert_eq!(code, Ok(0), "{out}");
        assert!(out.contains("10 cases lint clean"), "{out}");
        let (_, json) = run(&h, &["lint", "--json"]);
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(doc["problems"].as_array().unwrap().len(), 0);
    }
}
