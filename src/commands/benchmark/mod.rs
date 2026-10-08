//! `zirv benchmark`: measure which harness and model stack is best on this machine.
//!
//! Every candidate runs solo against a small embedded corpus; the stack is
//! composed afterwards from per-role scores (see `report`).

mod corpus;
mod discover;
mod report;
mod run;

use std::io::Write;

use clap::{Args, Parser, Subcommand};

use crate::commands::ctx::adapters::{self, Liveness};
use crate::commands::ctx::config::{CtxConfig, EnvLookup, env_from_process};
use crate::commands::ctx::models;
use crate::commands::ctx::price;
use crate::commands::ctx::state::{self, StateDir};

const DEFAULT_REPS: u32 = 1;
const DEFAULT_MAX_USD: f64 = 10.0;
const DEFAULT_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Parser)]
#[command(
    name = "zirv benchmark",
    about = "Measure which harness and model stack is best on this machine.",
    disable_help_subcommand = true,
    disable_version_flag = true,
    arg_required_else_help = true,
    subcommand_required = true
)]
pub struct BenchmarkCli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show what would run, without any model call.
    Plan {
        #[command(flatten)]
        filters: Filters,
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Run the benchmark. Spends real model quota.
    Run {
        #[command(flatten)]
        filters: Filters,
        /// Repetitions of every candidate and task.
        #[arg(long, default_value_t = DEFAULT_REPS, value_parser = clap::value_parser!(u32).range(1..))]
        reps: u32,
        /// Stop launching new runs once measured spend (agents and judge) reaches this many USD.
        #[arg(long, default_value_t = DEFAULT_MAX_USD, value_parser = parse_usd)]
        max_usd: f64,
        /// Wall-clock limit for one agent run.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
        timeout_secs: u64,
        /// Confirm that this spends real model quota.
        #[arg(long)]
        yes: bool,
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Re-render a stored run; the newest when no id is given.
    Report {
        run_id: Option<String>,
        /// Emit one JSON document.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Default, Args)]
pub struct Filters {
    /// Restrict to these harnesses.
    #[arg(long = "harness", value_name = "name")]
    pub harnesses: Vec<String>,
    /// Restrict discovered models to these model families (see `zirv ctx models`).
    #[arg(long = "family", value_name = "name")]
    pub families: Vec<String>,
    /// Benchmark exactly these model ids instead of the discovered ones. Pins are exact:
    /// `--family` does not filter them, and a pin on an unavailable harness is an error.
    #[arg(long = "model", value_name = "harness:model", value_parser = parse_pair)]
    pub models: Vec<(String, String)>,
    /// Restrict to these corpus tasks.
    #[arg(long = "task", value_name = "id")]
    pub tasks: Vec<String>,
    /// Override the judge.
    #[arg(long, value_name = "harness:model", value_parser = parse_pair, conflicts_with = "no_judge")]
    pub judge: Option<(String, String)>,
    /// Skip the LLM judge; deterministic graders only.
    #[arg(long)]
    pub no_judge: bool,
}

fn parse_pair(raw: &str) -> Result<(String, String), String> {
    match raw.split_once(':') {
        Some((harness, model)) if !harness.is_empty() && !model.is_empty() => {
            Ok((harness.to_string(), model.to_string()))
        }
        _ => Err("expected <harness>:<model>".to_string()),
    }
}

fn parse_usd(raw: &str) -> Result<f64, String> {
    match raw.parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd >= 0.0 => Ok(usd),
        _ => Err("expected a non-negative number of USD".to_string()),
    }
}

/// `args[0]` is the literal `benchmark` command as it appeared in argv. It is
/// discarded so case-insensitive raw dispatch retains stable clap usage text.
pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv benchmark".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match BenchmarkCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };
    let env = env_from_process();
    match execute(cli.command, &env, &adapters::liveness_probe) {
        Ok(code) => code,
        Err(error) => {
            crate::output::error(error);
            1
        }
    }
}

fn execute(
    command: Command,
    env: EnvLookup<'_>,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> Result<i32, String> {
    let repo = std::env::current_dir()
        .map_err(|e| format!("could not read the working directory: {e}"))?;
    let cfg = CtxConfig::load(&repo, env).map_err(|e| e.to_string())?;
    let python_present = adapters::program_is_present("python3");
    let stdout = &mut std::io::stdout();
    match command {
        Command::Plan { filters, json } => {
            let listing = model_listing(&cfg, env)?;
            let plan = match run::plan(
                &cfg,
                &filters,
                DEFAULT_REPS,
                DEFAULT_MAX_USD,
                python_present,
                present,
                &listing,
            ) {
                Ok(plan) => plan,
                Err(error) => return Ok(usage_error(error)),
            };
            emit(stdout, json, &plan, &run::render_plan(&plan))?;
            Ok(0)
        }
        Command::Report { run_id, json } => {
            let root = benchmark_root(env)?;
            let dir = run::find_run(&root, run_id.as_deref())?;
            let (meta, rows) = run::load_store(&dir)?;
            let report = report::build(meta, &rows);
            emit(stdout, json, &report, &report::render_text(&report))?;
            Ok(0)
        }
        Command::Run {
            filters,
            reps,
            max_usd,
            timeout_secs,
            yes,
            json,
        } => {
            let listing = model_listing(&cfg, env)?;
            let plan = match run::plan(
                &cfg,
                &filters,
                reps,
                max_usd,
                python_present,
                present,
                &listing,
            ) {
                Ok(plan) => plan,
                Err(error) => return Ok(usage_error(error)),
            };
            if !yes {
                emit(stdout, json, &plan, &run::render_plan(&plan))?;
                let message = "re-run with --yes to start; this spends real model quota";
                if json {
                    eprintln!("{message}");
                } else {
                    println!("\n{message}");
                }
                return Ok(2);
            }
            if plan.agent_runs == 0 {
                return Err("nothing to benchmark: no candidate or no runnable task".to_string());
            }
            let report = run_benchmark(&cfg, env, &plan, timeout_secs)?;
            emit(stdout, json, &report, &report::render_text(&report))?;
            Ok(0)
        }
    }
}

fn run_benchmark(
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    plan: &run::Plan,
    timeout_secs: u64,
) -> Result<report::Report, String> {
    // SAFETY: nothing else is running yet; the CLI is single-threaded until the first launch.
    // Child zirv hooks read their own process env, so the knobs must be set there too.
    unsafe {
        for (key, value) in run::CHILD_KNOBS {
            std::env::set_var(key, value);
        }
    }
    let started = chrono::Utc::now();
    let id = run::run_id(started);
    let dir = benchmark_root(env)?.join(&id);
    let temp = std::env::temp_dir();
    let temp = std::fs::canonicalize(&temp).unwrap_or(temp);
    let work_root = temp.join("zirv-benchmark").join(&id);
    let work = work_root.join("work");
    state::create_private_dir_all(&work_root)
        .map_err(|e| format!("{}: {e}", work_root.display()))?;

    let meta = plan.meta(
        &id,
        &started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    run::start_store(&dir, &meta)?;
    let table = price::resolve_table(cfg);
    let launcher = run::ExecLauncher {
        cfg,
        env,
        work: &work,
        table: &table,
        timeout_secs,
    };
    let mut launch = |spec: &run::LaunchSpec| launcher.launch(spec);
    let rows = run::Runner {
        dir: &dir,
        work: &work,
        python_present: plan.python3,
        git_present: adapters::program_is_present("git"),
        launch: &mut launch,
        progress: &mut |line| eprintln!("{line}"),
    }
    .execute(plan);
    let _ = std::fs::remove_dir_all(&work_root);
    Ok(report::build(meta, &rows?))
}

/// A filter that names something that cannot run is the caller's mistake, like a bad flag.
fn usage_error(error: String) -> i32 {
    crate::output::error(error);
    2
}

/// The runtime registry and the `zirv ctx models` rows (for prices and families).
fn model_listing(cfg: &CtxConfig, env: EnvLookup<'_>) -> Result<models::Listing, String> {
    let state = StateDir::resolve(env).map_err(|e| e.to_string())?;
    Ok(models::listing(cfg, &state))
}

fn benchmark_root(env: EnvLookup<'_>) -> Result<std::path::PathBuf, String> {
    let state = StateDir::resolve(env).map_err(|e| e.to_string())?;
    Ok(state.root().join("benchmark"))
}

fn emit(
    out: &mut dyn Write,
    json: bool,
    value: &impl serde::Serialize,
    text: &str,
) -> Result<(), String> {
    let rendered = if json {
        let mut document = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
        document.push('\n');
        document
    } else {
        text.to_string()
    };
    out.write_all(rendered.as_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::testenv;
    use std::collections::HashMap;
    use std::path::Path;

    fn parse(args: &[&str]) -> Result<BenchmarkCli, clap::Error> {
        BenchmarkCli::try_parse_from(std::iter::once("zirv benchmark").chain(args.iter().copied()))
    }

    #[test]
    fn a_bare_benchmark_prints_help_and_a_subcommand_is_required() {
        let error = parse(&[]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn filters_repeat_and_parse() {
        let cli = parse(&[
            "run",
            "--harness",
            "claude",
            "--harness",
            "codex",
            "--family",
            "alpha",
            "--family",
            "beta",
            "--model",
            "claude:vendor-model-2",
            "--task",
            "w-read",
            "--judge",
            "codex:m",
            "--reps",
            "3",
            "--max-usd",
            "2.5",
            "--timeout-secs",
            "30",
            "--yes",
            "--json",
        ])
        .unwrap();
        let Command::Run {
            filters,
            reps,
            max_usd,
            timeout_secs,
            yes,
            json,
        } = cli.command
        else {
            panic!("expected run");
        };
        assert_eq!(filters.harnesses, vec!["claude", "codex"]);
        assert_eq!(filters.families, vec!["alpha", "beta"]);
        assert_eq!(
            filters.models,
            vec![("claude".to_string(), "vendor-model-2".to_string())]
        );
        assert_eq!(filters.judge, Some(("codex".to_string(), "m".to_string())));
        assert_eq!(
            (reps, max_usd, timeout_secs, yes, json),
            (3, 2.5, 30, true, true)
        );
    }

    #[test]
    fn run_defaults_match_the_spec() {
        let Command::Run {
            reps,
            max_usd,
            timeout_secs,
            yes,
            ..
        } = parse(&["run"]).unwrap().command
        else {
            panic!("expected run");
        };
        assert_eq!((reps, max_usd, timeout_secs, yes), (1, 10.0, 600, false));
    }

    #[test]
    fn a_model_pin_on_a_harness_that_cannot_run_exits_2() {
        let tmp = testenv::repo();
        let _home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let env: HashMap<&str, String> = [(
            state::STATE_ENV,
            tmp.path().join("state").display().to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();
        let _cwd = testenv::CwdGuard::enter(tmp.path()).unwrap();
        let command = parse(&["plan", "--model", "goose:vendor-model-2"])
            .unwrap()
            .command;
        let code = execute(command, &lookup, &adapters::only_installed(&["claude"]));
        assert_eq!(code, Ok(2));
    }

    #[test]
    fn bad_values_are_rejected() {
        assert!(parse(&["run", "--tier", "deep"]).is_err());
        assert!(parse(&["run", "--model", "nocolon"]).is_err());
        assert!(parse(&["run", "--reps", "0"]).is_err());
        assert!(parse(&["run", "--max-usd", "-1"]).is_err());
        assert!(parse(&["run", "--judge", "a:b", "--no-judge"]).is_err());
        assert!(parse(&["plan", "--yes"]).is_err());
    }

    #[test]
    fn run_without_yes_prints_the_plan_exits_2_and_launches_nothing() {
        let tmp = testenv::repo();
        let _home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let argv_log = tmp.path().join("argv.log");
        let fake = format!(
            "sh {}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake-agent.sh")
                .display()
        );
        let env: HashMap<&str, String> = [
            (
                state::STATE_ENV,
                tmp.path().join("state").display().to_string(),
            ),
            ("ZIRV_CTX_AGENT_BIN", fake),
        ]
        .into();
        let lookup = |key: &str| env.get(key).cloned();
        // SAFETY: CI runs tests single-threaded.
        unsafe { std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log) };
        let _cwd = testenv::CwdGuard::enter(tmp.path()).unwrap();
        let command = parse(&["run", "--no-judge", "--task", "w-read"])
            .unwrap()
            .command;
        let code = execute(command, &lookup, &adapters::only_installed(&["claude"]));
        unsafe { std::env::remove_var("FAKE_AGENT_ARGV_LOG") };

        assert_eq!(code, Ok(2));
        assert!(!argv_log.exists(), "no agent may be launched without --yes");
        assert!(
            !tmp.path().join("state/benchmark").exists(),
            "nothing is stored without --yes"
        );
    }
}
