//! `zirv benchmark eval`: behaviour cases that grade what an agent does (its tool calls), not
//! what it says. This tier is free: `lint`, `plan` and `run --dry-run` never launch a harness.

mod corpus;
mod grade;
mod trace;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use clap::Subcommand;
use serde_json::json;

use crate::commands::ctx::config::EnvLookup;

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
    /// Show what a replay would send. Only `--dry-run` is implemented; it spends nothing.
    Run {
        /// Print the resolved surface text, turns and bundle hash; launch nothing.
        #[arg(long)]
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
    },
}

pub(super) fn execute(
    command: EvalCommand,
    repo: &Path,
    env: EnvLookup<'_>,
    out: &mut dyn Write,
) -> Result<i32, String> {
    let cases = corpus::embedded()?;
    match command {
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
        } => {
            if !dry_run {
                eprintln!(
                    "paid replay not implemented yet; use `zirv benchmark eval run --dry-run`"
                );
                return Ok(2);
            }
            if let Some(unknown) = wanted.iter().find(|id| !cases.iter().any(|c| &c.id == *id)) {
                crate::output::error(format!("unknown eval case '{unknown}'"));
                return Ok(2);
            }
            dry_run_report(&cases, &wanted, repo, env, out, context, json)
        }
    }
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
        &|surface| corpus::resolve_surface(surface, repo, env).map(|_| ()),
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
    let mut resolved: BTreeMap<&str, corpus::Resolved> = BTreeMap::new();
    let mut rows = Vec::new();
    let mut text = String::new();
    for case in cases
        .iter()
        .filter(|c| wanted.is_empty() || wanted.contains(&c.id))
    {
        if !resolved.contains_key(case.surface.as_str()) {
            let surface = corpus::resolve_surface(&case.surface, repo, env)?;
            resolved.insert(case.surface.as_str(), surface);
        }
        let surface = &resolved[case.surface.as_str()];
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

    #[test]
    fn run_without_dry_run_exits_2_and_spawns_nothing() {
        let h = harness();
        let (code, out) = run(&h, &["run"]);
        assert_eq!(code, Ok(2));
        assert!(out.is_empty());
        assert!(!h.argv_log.exists(), "no agent may be launched");
        assert!(!h.tmp.path().join("state/benchmark").exists());
        assert!(!h.tmp.path().join("state/eval").exists());
        // `--context`, `--case` and `--json` do not make it a paid replay either.
        let (code, _) = run(
            &h,
            &[
                "run",
                "--context",
                "--json",
                "--case",
                "skill-load-via-shell",
            ],
        );
        assert_eq!(code, Ok(2));
        assert!(!h.argv_log.exists());
    }

    #[test]
    fn dry_run_prints_the_surface_turns_and_bundle_hash() {
        let h = harness();
        let (code, out) = run(&h, &["run", "--dry-run", "--case", "no-unprompted-spend"]);
        assert_eq!(code, Ok(0));
        let resolved = corpus::resolve_surface(
            "safety:Bash(zirv benchmark run*)",
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
