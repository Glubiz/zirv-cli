//! `zirv workflow research plan`: full validation and resolution with ZERO
//! backend or provider calls -- baseline sha, corpus split counts,
//! candidate scope/env validation, evaluator fingerprint, a schedule
//! preview against the campaign's own budgets, and coverage notes. Exit 0
//! when the campaign could run as declared, exit 2 when it is refused.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use super::corpus::Corpus;
use super::guard;
use super::manifest::{self, Manifest};
use super::reconcile::resolve_protected_paths;
use crate::commands::ctx::CtxResult;

#[derive(Debug, Clone, Serialize)]
pub struct SplitCounts {
    pub dev: usize,
    pub validation: usize,
    pub holdout: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidatePlanRow {
    pub id: String,
    pub hypothesis: String,
    pub env_keys: usize,
    pub has_patch: bool,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchedulePreview {
    pub baseline_trials: usize,
    pub worst_case_trials: usize,
    pub worst_case_spend_usd: f64,
    pub worst_case_calls: u64,
    pub worst_case_wall_secs: u64,
    pub fits_budgets: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanReport {
    pub id: String,
    pub baseline_sha: String,
    pub evaluator_fingerprint: String,
    pub splits: SplitCounts,
    pub schedule: SchedulePreview,
    pub candidates: Vec<CandidatePlanRow>,
    pub coverage: Vec<String>,
    pub valid: bool,
    pub errors: Vec<String>,
}

fn resolve_git_sha(repo: &Path, commit: &str) -> CtxResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg(commit)
        .output()
        .map_err(|err| format!("could not run `git rev-parse {commit}`: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git rev-parse {commit}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `git apply --check` against a disposable, detached worktree at the
/// baseline commit -- removed immediately after, whatever the result. Never
/// touches the caller's actual working tree. Built under the system temp
/// dir with a pid+time-based name rather than the `tempfile` crate (a dev
/// dependency only) since this path runs in production code, not tests.
fn check_patch_applies(repo: &Path, baseline_sha: &str, patch_path: &Path) -> Result<(), String> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let wt_dir = std::env::temp_dir().join(format!(
        "zirv-research-plan-{}-{unique}",
        std::process::id()
    ));
    let add = Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg(&wt_dir)
        .arg(baseline_sha)
        .current_dir(repo)
        .status()
        .map_err(|err| format!("could not run `git worktree add`: {err}"))?;
    if !add.success() {
        return Err("`git worktree add` failed while checking a patch".to_string());
    }
    let check = Command::new("git")
        .arg("apply")
        .arg("--check")
        .arg(patch_path)
        .current_dir(&wt_dir)
        .status();
    let _ = Command::new("git")
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(&wt_dir)
        .current_dir(repo)
        .status();
    match check {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("`git apply --check` failed ({status})")),
        Err(err) => Err(format!("could not run `git apply --check`: {err}")),
    }
}

fn candidate_plan_row(
    repo: &Path,
    baseline_sha: &str,
    manifest: &Manifest,
    candidate: &manifest::Candidate,
    manifest_dir: &Path,
    protected: &[String],
) -> CandidatePlanRow {
    let mut problems = Vec::new();

    if let Some(patch_rel) = &candidate.patch {
        let Some(source_patch) = &manifest.candidate_space.source_patch else {
            problems.push("has a patch but [candidate_space.source_patch] is absent".to_string());
            return CandidatePlanRow {
                id: candidate.id.clone(),
                hypothesis: candidate.hypothesis.clone(),
                env_keys: candidate.env.len(),
                has_patch: true,
                problems,
            };
        };
        let patch_path = manifest_dir.join(patch_rel);
        if !patch_path.is_file() {
            problems.push(format!("patch file '{}' not found", patch_path.display()));
        } else {
            match Command::new("git")
                .arg("apply")
                .arg("--numstat")
                .arg(&patch_path)
                .current_dir(repo)
                .output()
            {
                Ok(output) if output.status.success() => {
                    let touched = guard::parse_numstat(&String::from_utf8_lossy(&output.stdout));
                    let violations = guard::validate_patch_scope(
                        &touched,
                        &source_patch.allowed_paths,
                        protected,
                    );
                    for violation in violations {
                        problems.push(format!("{}: {}", violation.path, violation.reason));
                    }
                    if problems.is_empty()
                        && let Err(reason) = check_patch_applies(repo, baseline_sha, &patch_path)
                    {
                        problems.push(reason);
                    }
                }
                Ok(output) => problems.push(format!(
                    "`git apply --numstat` failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(err) => problems.push(format!("could not run `git apply --numstat`: {err}")),
            }
        }
    }

    CandidatePlanRow {
        id: candidate.id.clone(),
        hypothesis: candidate.hypothesis.clone(),
        env_keys: candidate.env.len(),
        has_patch: candidate.patch.is_some(),
        problems,
    }
}

pub fn plan(manifest_path: &Path, repo: &Path) -> CtxResult<PlanReport> {
    let manifest = Manifest::load(manifest_path)?;
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut errors = Vec::new();

    let baseline_sha = match resolve_git_sha(repo, &manifest.baseline.commit) {
        Ok(sha) => sha,
        Err(err) => {
            errors.push(err.to_string());
            String::new()
        }
    };

    let protected_paths = resolve_protected_paths(repo, &manifest).unwrap_or_default();
    let (_, evaluator_fingerprint) = match guard::hash_protected_files(repo, &protected_paths) {
        Ok(pair) => pair,
        Err(err) => {
            errors.push(err.to_string());
            (std::collections::BTreeMap::new(), String::new())
        }
    };

    let corpus_path = repo.join(&manifest.corpus.file);
    let loaded_corpus = Corpus::load(&corpus_path);
    let splits = match &loaded_corpus {
        Ok(corpus) => SplitCounts {
            dev: corpus.tasks_for_split(manifest::Split::Dev).len(),
            validation: corpus.tasks_for_split(manifest::Split::Validation).len(),
            holdout: corpus.tasks_for_split(manifest::Split::Holdout).len(),
        },
        Err(err) => {
            errors.push(err.to_string());
            SplitCounts {
                dev: 0,
                validation: 0,
                holdout: 0,
            }
        }
    };

    let protected_strs: Vec<String> = protected_paths
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let candidates: Vec<CandidatePlanRow> = manifest
        .candidates
        .iter()
        .map(|candidate| {
            candidate_plan_row(
                repo,
                &baseline_sha,
                &manifest,
                candidate,
                &manifest_dir,
                &protected_strs,
            )
        })
        .collect();
    for row in &candidates {
        for problem in &row.problems {
            errors.push(format!("candidate '{}': {problem}", row.id));
        }
    }

    // Issue #804: a stage's own `classes` filter (if any) narrows which
    // tasks of its split actually run -- the same
    // `tasks_for_split_and_classes` the runner itself dispatches from, so
    // this preview and the real schedule can never disagree.
    let class_filtered_count = |split: manifest::Split, classes: &[String]| -> usize {
        loaded_corpus
            .as_ref()
            .map(|corpus| corpus.tasks_for_split_and_classes(split, classes).len())
            .unwrap_or(0)
    };
    let screen_task_count = class_filtered_count(
        manifest.stages.screen.split,
        &manifest.stages.screen.classes,
    );
    let validate_task_count = class_filtered_count(
        manifest.stages.validate.split,
        &manifest.stages.validate.classes,
    );
    let holdout_task_count = class_filtered_count(
        manifest.stages.holdout.split,
        &manifest.stages.holdout.classes,
    );
    let screen_trials = screen_task_count * manifest.stages.screen.reps as usize;
    let validate_trials = validate_task_count * manifest.stages.validate.reps as usize;
    let holdout_trials = holdout_task_count * manifest.stages.holdout.reps as usize;
    let baseline_trials = screen_trials + validate_trials + holdout_trials;
    // Every declared candidate runs its own screen + validate trials paired
    // against the shared baseline; at most one (the winner) also reaches
    // holdout, so the holdout term is added once, not per candidate.
    let candidate_worst_case =
        manifest.candidates.len() * (screen_trials + validate_trials) + holdout_trials;
    let worst_case_trials = baseline_trials + candidate_worst_case;
    let worst_case_spend_usd = worst_case_trials as f64 * manifest.backend.per_trial_ceiling_usd;
    let worst_case_calls = worst_case_trials as u64 * manifest.backend.calls_per_trial as u64;
    let worst_case_wall_secs = (worst_case_trials as u64 * manifest.backend.timeout_secs)
        / manifest.budgets.concurrency.max(1) as u64;

    let baseline_fits = baseline_trials as f64 * manifest.backend.per_trial_ceiling_usd
        <= manifest.budgets.max_spend_usd
        && baseline_trials as u64 <= manifest.budgets.max_trials;
    if !baseline_fits {
        errors.push(
            "the baseline stages alone cannot fit inside budgets.max_spend_usd/max_trials"
                .to_string(),
        );
    }
    let fits_budgets = worst_case_spend_usd <= manifest.budgets.max_spend_usd
        && worst_case_calls <= manifest.budgets.max_calls
        && worst_case_trials as u64 <= manifest.budgets.max_trials;

    // Issue #802/#804: a stage that can never produce `criteria.min_pairs`
    // pairs can never promote a candidate at all -- `evaluate_cohort`
    // returns Inconclusive below that floor, forever, on every run. Catch
    // it at plan time rather than after real trials have already spent
    // money. With `stratify = "class"`, the gate never pools cohorts, so
    // the binding constraint is the SMALLEST class's own pair count, not
    // the stage's total.
    let min_pairs = manifest.criteria.min_pairs;
    for (label, split, classes, reps) in [
        (
            "validate",
            manifest.stages.validate.split,
            &manifest.stages.validate.classes,
            manifest.stages.validate.reps,
        ),
        (
            "holdout",
            manifest.stages.holdout.split,
            &manifest.stages.holdout.classes,
            manifest.stages.holdout.reps,
        ),
    ] {
        let Ok(corpus) = &loaded_corpus else { continue };
        let worst_case_tasks = if matches!(manifest.stratify, manifest::Stratify::Class) {
            corpus
                .class_counts_for_split(split, classes)
                .values()
                .copied()
                .min()
                .unwrap_or(0)
        } else {
            corpus.tasks_for_split_and_classes(split, classes).len()
        };
        let pairs = worst_case_tasks * reps as usize;
        if pairs < min_pairs {
            errors.push(format!(
                "stage {label} yields {pairs} pairs < criteria.min_pairs={min_pairs}: no candidate could ever be promoted"
            ));
        }
    }

    let mut coverage = Vec::new();
    if matches!(manifest.runtime, manifest::Runtime::Native) {
        coverage.push(
            "runtime = \"native\": zirv native is release-gated; this campaign is unmeasured."
                .to_string(),
        );
    }
    if matches!(manifest.seat_mode, manifest::SeatMode::Orchestration) {
        coverage.push("seat_mode = \"orchestration\": no orchestration suite exists yet; this campaign is unmeasured.".to_string());
    }
    if let Ok(corpus) = Corpus::load(&corpus_path)
        && corpus.is_single_family()
    {
        coverage.push(
            "corpus has a single task family: results generalize to that family only.".to_string(),
        );
    }

    let valid = errors.is_empty() && baseline_fits;

    Ok(PlanReport {
        id: manifest.id.clone(),
        baseline_sha,
        evaluator_fingerprint,
        splits,
        schedule: SchedulePreview {
            baseline_trials,
            worst_case_trials,
            worst_case_spend_usd,
            worst_case_calls,
            worst_case_wall_secs,
            fits_budgets,
        },
        candidates,
        coverage,
        valid,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as Cmd;

    fn init_git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            assert!(
                Cmd::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn write_manifest(dir: &Path) -> PathBuf {
        std::fs::write(
            dir.join("corpus.toml"),
            r#"
schema = 1
version = "1"

[[task]]
id = "t1"
family = "f"
class = "bounded"
split = "dev"
"#,
        )
        .unwrap();
        std::fs::write(dir.join("fixture.toml"), "").unwrap();
        let manifest_path = dir.join("manifest.toml");
        std::fs::write(
            &manifest_path,
            r#"
schema = 1
id = "plan-demo"
runtime = "meta"
seat_mode = "single"
cache_mode = "cold"
billing = "subscription"

[baseline]
commit = "HEAD"

[corpus]
file = "corpus.toml"

[backend]
kind = "fixture"
file = "fixture.toml"
per_trial_ceiling_usd = 1.0
calls_per_trial = 2
timeout_secs = 30

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 100.0
max_wall_secs = 3600
max_calls = 200
max_trials = 200
max_retries = 0
concurrency = 1

[stages.screen]
split = "dev"
reps = 1

[stages.validate]
split = "validation"
reps = 1

[stages.holdout]
split = "holdout"
reps = 1
max_uses = 1

[criteria]
min_pairs = 0
"#,
        )
        .unwrap();
        manifest_path
    }

    #[test]
    fn plan_never_dispatches_a_backend_call() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let manifest_path = write_manifest(repo.path());

        super::super::backend::reset_fixture_dispatch_count();
        let report = plan(&manifest_path, repo.path()).unwrap();
        assert!(report.valid, "errors: {:?}", report.errors);
        assert_eq!(
            super::super::backend::fixture_dispatch_count(),
            0,
            "plan must never run a trial"
        );
    }

    #[test]
    fn plan_refuses_when_the_baseline_alone_cannot_fit_the_spend_cap() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let manifest_path = write_manifest(repo.path());
        let text = std::fs::read_to_string(&manifest_path).unwrap();
        let text = text.replace("max_spend_usd = 100.0", "max_spend_usd = 0.0");
        std::fs::write(&manifest_path, text).unwrap();

        let report = plan(&manifest_path, repo.path()).unwrap();
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("baseline")));
    }

    /// Issue #802/#804: a validate/holdout stage that can never produce
    /// `criteria.min_pairs` pairs can never promote a candidate -- `plan`
    /// must refuse before any real trial spends money finding that out the
    /// slow way.
    #[test]
    fn plan_refuses_a_stage_that_can_never_reach_min_pairs() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let manifest_path = write_manifest(repo.path());
        // `write_manifest`'s corpus has exactly one `dev` task and none in
        // `validation`/`holdout` -- raise min_pairs above 0 so the stage
        // yields fewer pairs than required.
        let text = std::fs::read_to_string(&manifest_path).unwrap();
        let text = text.replace("min_pairs = 0", "min_pairs = 1");
        std::fs::write(&manifest_path, text).unwrap();

        let report = plan(&manifest_path, repo.path()).unwrap();
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e == "stage validate yields 0 pairs < criteria.min_pairs=1: no candidate could ever be promoted"),
            "got: {:?}",
            report.errors
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e == "stage holdout yields 0 pairs < criteria.min_pairs=1: no candidate could ever be promoted"),
            "got: {:?}",
            report.errors
        );
    }

    /// Every manifest under `docs/benchmarks/autoresearch/campaigns/` (the
    /// real, hand-authored campaigns plus the fixture demo), planned against
    /// this actual worktree -- never a synthetic repo -- so
    /// `corpus.file`/`evaluator.protected`/`backend.command` really resolve
    /// or really fail exactly as a real `zirv workflow research plan` run
    /// would see them. Every committed manifest must be genuinely runnable:
    /// `plan` must not skip one silently, and none may be refused -- each
    /// real campaign's stage splits/classes/reps/budgets are tuned so its
    /// baseline fits with room to screen every candidate, and every
    /// validate/holdout stage clears `criteria.min_pairs`.
    #[test]
    fn plan_covers_every_committed_campaign_manifest_without_dispatching() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let campaigns_dir = repo.join("docs/benchmarks/autoresearch/campaigns");
        let mut manifests: Vec<PathBuf> = std::fs::read_dir(&campaigns_dir)
            .expect("campaigns dir must exist")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("toml"))
            .collect();
        manifests.sort();

        let expected: &[&str] = &[
            "context-compaction-forced.toml",
            "context-compaction.toml",
            "fixture-demo.toml",
            "jev-determinism-classify.toml",
            "jev-determinism-compaction_select.toml",
            "jev-determinism-context.toml",
            "jev-determinism-dispatch.toml",
            "jev-determinism-handoff_select.toml",
            "jev-determinism-handoff_thin.toml",
            "jev-determinism-harvest_screen.toml",
            "jev-determinism-inject.toml",
            "jev-determinism-intake.toml",
            "jev-determinism-launch_effort.toml",
            "jev-determinism-memory.toml",
            "jev-gates-e2e.toml",
            "jev-intake-floors.toml",
            "routing-ladder.toml",
        ];
        assert_eq!(
            manifests.len(),
            expected.len(),
            "a new/removed campaign manifest must update this test's expectations, not be skipped: {manifests:?}"
        );

        super::super::backend::reset_fixture_dispatch_count();
        for manifest_path in &manifests {
            let name = manifest_path.file_name().unwrap().to_str().unwrap();
            assert!(
                expected.contains(&name),
                "{name}: not in the expectation list -- update this test, don't skip it"
            );
            let report = plan(manifest_path, repo)
                .unwrap_or_else(|err| panic!("plan({name}) must not error outright: {err}"));
            assert!(
                report.valid,
                "{name} was expected valid; errors: {:?}",
                report.errors
            );
        }
        assert_eq!(
            super::super::backend::fixture_dispatch_count(),
            0,
            "plan must never run a trial, including fixture-demo.toml's fixture backend"
        );
    }
}
