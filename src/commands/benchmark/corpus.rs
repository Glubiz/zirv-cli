//! The embedded benchmark corpus: role-tagged tasks, the fixture repository
//! they run against, and the deterministic graders that score an answer.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::commands::ctx::models::evidence::{CellComplexity, RouteRole};
use crate::commands::ctx::result_schema::extract_json_candidate;
use crate::commands::ctx::supervise;

const CORPUS_SCHEMA: u32 = 1;
const PYTHON_TIMEOUT: Duration = Duration::from_secs(30);

const CORPUS_TOML: &str = include_str!("corpus/corpus.toml");

/// Fixed list, so the binary carries exactly the files a run materializes.
const FIXTURE: [(&str, &str); 9] = [
    ("README.md", include_str!("corpus/fixture/README.md")),
    (
        "tinyshop/__init__.py",
        include_str!("corpus/fixture/tinyshop/__init__.py"),
    ),
    (
        "tinyshop/util.py",
        include_str!("corpus/fixture/tinyshop/util.py"),
    ),
    (
        "tinyshop/pricing.py",
        include_str!("corpus/fixture/tinyshop/pricing.py"),
    ),
    (
        "tinyshop/inventory.py",
        include_str!("corpus/fixture/tinyshop/inventory.py"),
    ),
    (
        "tests/__init__.py",
        include_str!("corpus/fixture/tests/__init__.py"),
    ),
    (
        "tests/test_pricing.py",
        include_str!("corpus/fixture/tests/test_pricing.py"),
    ),
    ("changes.diff", include_str!("corpus/fixture/changes.diff")),
    (
        "ci-failure.log",
        include_str!("corpus/fixture/ci-failure.log"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Orchestrator,
    Worker,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Orchestrator => "orchestrator",
            Role::Worker => "worker",
        }
    }

    /// The routing role an untagged task of this role is evidence for.
    pub fn route_role(self) -> RouteRole {
        match self {
            Role::Orchestrator => RouteRole::Orchestrator,
            Role::Worker => RouteRole::Worker,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Grader {
    AnswerRegex {
        pattern: String,
    },
    FileRegex {
        path: String,
        pattern: String,
        #[serde(default)]
        absent: bool,
    },
    AnswerJson {
        required: Vec<String>,
    },
    Python {
        code: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub role: Role,
    /// How hard the task is; required so every probe lands in a routing cell.
    pub complexity: CellComplexity,
    /// The routing role this task is evidence for when it differs from `role`.
    #[serde(default)]
    pub route_role: Option<RouteRole>,
    #[serde(default = "default_judge")]
    pub judge: bool,
    pub prompt: String,
    #[serde(default, rename = "grader")]
    pub graders: Vec<Grader>,
}

impl Task {
    /// The routing role this task is evidence for: its explicit `route_role`, else its `role`.
    pub fn routing_role(&self) -> RouteRole {
        self.route_role.unwrap_or_else(|| self.role.route_role())
    }
}

fn default_judge() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    schema: u32,
    #[serde(rename = "task")]
    pub tasks: Vec<Task>,
}

/// Graders that passed and graders that ran; skipped graders count in neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Grade {
    pub passed: u32,
    pub evaluated: u32,
}

impl Grade {
    pub fn correctness(self) -> Option<f64> {
        (self.evaluated > 0).then(|| f64::from(self.passed) / f64::from(self.evaluated))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    Skip,
}

pub fn embedded() -> Result<Corpus, String> {
    parse(CORPUS_TOML)
}

pub fn parse(text: &str) -> Result<Corpus, String> {
    let corpus: Corpus = toml::from_str(text).map_err(|error| format!("corpus: {error}"))?;
    if corpus.schema != CORPUS_SCHEMA {
        return Err(format!(
            "corpus: schema {} is not supported (expected {CORPUS_SCHEMA})",
            corpus.schema
        ));
    }
    let mut seen = BTreeSet::new();
    for task in &corpus.tasks {
        if !seen.insert(task.id.as_str()) {
            return Err(format!("corpus: duplicate task id '{}'", task.id));
        }
        if task.graders.is_empty() {
            return Err(format!("corpus: task '{}' has no grader", task.id));
        }
        for grader in &task.graders {
            if let Grader::AnswerRegex { pattern } | Grader::FileRegex { pattern, .. } = grader {
                compile(pattern).map_err(|error| format!("corpus: task '{}': {error}", task.id))?;
            }
        }
    }
    Ok(corpus)
}

fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern).case_insensitive(true).build()
}

/// Write the fixture into `dir`, which the caller has emptied.
pub fn materialize(dir: &Path) -> std::io::Result<()> {
    for (relative, contents) in FIXTURE {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Why `task` cannot run, or `None` when at least one grader can.
pub fn skip_reason(task: &Task, python_present: bool) -> Option<&'static str> {
    let python_only = task
        .graders
        .iter()
        .all(|grader| matches!(grader, Grader::Python { .. }));
    (python_only && !python_present).then_some("python3 not found")
}

pub fn grade(task: &Task, answer: &str, workdir: &Path, python_present: bool) -> Grade {
    let mut grade = Grade::default();
    for grader in &task.graders {
        match grade_one(grader, answer, workdir, python_present) {
            Verdict::Pass => {
                grade.passed += 1;
                grade.evaluated += 1;
            }
            Verdict::Fail => grade.evaluated += 1,
            Verdict::Skip => {}
        }
    }
    grade
}

fn grade_one(grader: &Grader, answer: &str, workdir: &Path, python_present: bool) -> Verdict {
    let passed = match grader {
        Grader::AnswerRegex { pattern } => compile(pattern).is_ok_and(|re| re.is_match(answer)),
        Grader::FileRegex {
            path,
            pattern,
            absent,
        } => match std::fs::read_to_string(workdir.join(path)) {
            Ok(contents) => compile(pattern).is_ok_and(|re| re.is_match(&contents) != *absent),
            Err(_) => false,
        },
        Grader::AnswerJson { required } => answer_has_arrays(answer, required),
        Grader::Python { code } => {
            if !python_present {
                return Verdict::Skip;
            }
            python_passes(code, workdir, PYTHON_TIMEOUT)
        }
    };
    if passed { Verdict::Pass } else { Verdict::Fail }
}

fn answer_has_arrays(answer: &str, required: &[String]) -> bool {
    let Some(candidate) = extract_json_candidate(answer) else {
        return false;
    };
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str(&candidate) else {
        return false;
    };
    required.iter().all(|key| {
        object
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| !items.is_empty())
    })
}

/// Runs agent-written code, so it gets a scrubbed environment and its own process group.
fn python_passes(code: &str, workdir: &Path, timeout: Duration) -> bool {
    let mut command = Command::new("python3");
    command
        .args(["-I", "-c", code])
        .current_dir(workdir)
        .env_clear()
        .env("HOME", workdir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for keep in ["PATH", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(keep) {
            command.env(keep, value);
        }
    }
    supervise::isolate_process_tree(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The leader is reaped, but its group may still hold forked grandchildren.
                #[cfg(unix)]
                // SAFETY: same group signal `supervise::terminate_group` sends; ESRCH is ignored.
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                return status.success();
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = supervise::terminate_group(&mut child, Duration::from_millis(500));
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(graders: &str) -> Task {
        let text = format!(
            "schema = 1\n[[task]]\nid = \"t\"\nrole = \"worker\"\ncomplexity = \"bounded\"\nprompt = \"p\"\n{graders}"
        );
        parse(&text).expect("parses").tasks.remove(0)
    }

    fn graded(task: &Task, answer: &str, dir: &Path, python: bool) -> Grade {
        grade(task, answer, dir, python)
    }

    #[test]
    fn answer_regex_is_case_insensitive() {
        let t = task("[[task.grader]]\ntype = \"answer_regex\"\npattern = '\\breserve\\b'\n");
        let dir = tempfile::tempdir().unwrap();
        let g = graded(&t, "It is RESERVE here", dir.path(), true);
        assert_eq!(
            g,
            Grade {
                passed: 1,
                evaluated: 1
            }
        );
        let g = graded(&t, "reserved", dir.path(), true);
        assert_eq!(
            g,
            Grade {
                passed: 0,
                evaluated: 1
            }
        );
    }

    #[test]
    fn file_regex_passes_on_a_match_and_inverts_with_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.py"), "def line_subtotal(x): pass\n").unwrap();
        let present = task(
            "[[task.grader]]\ntype = \"file_regex\"\npath = \"f.py\"\npattern = 'def line_subtotal\\('\n",
        );
        assert_eq!(graded(&present, "", dir.path(), true).passed, 1);
        let absent = task(
            "[[task.grader]]\ntype = \"file_regex\"\npath = \"f.py\"\npattern = 'calc_subtotal'\nabsent = true\n",
        );
        assert_eq!(graded(&absent, "", dir.path(), true).passed, 1);
        let still_there = task(
            "[[task.grader]]\ntype = \"file_regex\"\npath = \"f.py\"\npattern = 'line_subtotal'\nabsent = true\n",
        );
        assert_eq!(graded(&still_there, "", dir.path(), true).passed, 0);
    }

    #[test]
    fn file_regex_on_a_missing_file_fails_even_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let t = task(
            "[[task.grader]]\ntype = \"file_regex\"\npath = \"nope.py\"\npattern = 'x'\nabsent = true\n",
        );
        assert_eq!(
            graded(&t, "", dir.path(), true),
            Grade {
                passed: 0,
                evaluated: 1
            }
        );
    }

    #[test]
    fn answer_json_needs_every_required_key_as_a_non_empty_array() {
        let t =
            task("[[task.grader]]\ntype = \"answer_json\"\nrequired = [\"files\", \"steps\"]\n");
        let dir = tempfile::tempdir().unwrap();
        let ok = "Here: {\"files\": [\"a\"], \"steps\": [1, 2]}";
        assert_eq!(graded(&t, ok, dir.path(), true).passed, 1);
        let empty = "{\"files\": [\"a\"], \"steps\": []}";
        assert_eq!(graded(&t, empty, dir.path(), true).passed, 0);
        let missing = "{\"files\": [\"a\"]}";
        assert_eq!(graded(&t, missing, dir.path(), true).passed, 0);
        let not_array = "{\"files\": \"a\", \"steps\": [1]}";
        assert_eq!(graded(&t, not_array, dir.path(), true).passed, 0);
        assert_eq!(graded(&t, "no json here", dir.path(), true).passed, 0);
    }

    #[test]
    fn python_graders_are_skipped_without_python3() {
        let t = task(
            "[[task.grader]]\ntype = \"answer_regex\"\npattern = 'x'\n[[task.grader]]\ntype = \"python\"\ncode = \"pass\"\n",
        );
        let dir = tempfile::tempdir().unwrap();
        let g = graded(&t, "x", dir.path(), false);
        assert_eq!(
            g,
            Grade {
                passed: 1,
                evaluated: 1
            }
        );
        assert_eq!(g.correctness(), Some(1.0));
    }

    #[test]
    fn a_task_of_only_python_graders_is_skipped_without_python3() {
        let only = task("[[task.grader]]\ntype = \"python\"\ncode = \"pass\"\n");
        assert_eq!(skip_reason(&only, false), Some("python3 not found"));
        assert_eq!(skip_reason(&only, true), None);
        let mixed = task(
            "[[task.grader]]\ntype = \"python\"\ncode = \"pass\"\n[[task.grader]]\ntype = \"answer_regex\"\npattern = 'x'\n",
        );
        assert_eq!(skip_reason(&mixed, false), None);
    }

    #[test]
    fn no_evaluated_graders_means_no_correctness() {
        assert_eq!(Grade::default().correctness(), None);
        assert_eq!(
            Grade {
                passed: 1,
                evaluated: 4
            }
            .correctness(),
            Some(0.25)
        );
    }

    #[test]
    fn the_python_grader_does_not_inherit_the_operators_environment() {
        if !crate::commands::ctx::adapters::program_is_present("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: CI runs tests single-threaded.
        unsafe { std::env::set_var("ZIRV_BENCH_SENTINEL", "secret") };
        let code = "import os, sys\nsys.exit(0 if 'ZIRV_BENCH_SENTINEL' not in os.environ and os.path.realpath(os.environ.get('HOME', '')) == os.path.realpath(os.getcwd()) else 1)";
        let clean = python_passes(code, dir.path(), PYTHON_TIMEOUT);
        unsafe { std::env::remove_var("ZIRV_BENCH_SENTINEL") };
        assert!(
            clean,
            "the sentinel leaked into the grader or HOME was not the workdir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_grader_takes_its_grandchildren_down_with_it() {
        if !crate::commands::ctx::adapters::program_is_present("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let code = format!(
            "import subprocess, time\np = subprocess.Popen(['sleep', '60'])\nopen({:?}, 'w').write(str(p.pid))\ntime.sleep(60)",
            pid_file.display().to_string()
        );
        assert!(!python_passes(&code, dir.path(), Duration::from_secs(2)));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            // SAFETY: signal 0 only probes for existence.
            unsafe { libc::kill(pid, 0) == -1 }
        });
        assert!(gone, "the grandchild outlived the timeout");
    }

    #[cfg(unix)]
    #[test]
    fn a_grader_that_exits_normally_does_not_leave_grandchildren_behind() {
        if !crate::commands::ctx::adapters::program_is_present("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let code = format!(
            "import subprocess\np = subprocess.Popen(['sleep', '60'])\nopen({:?}, 'w').write(str(p.pid))",
            pid_file.display().to_string()
        );
        assert!(python_passes(&code, dir.path(), PYTHON_TIMEOUT));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            // SAFETY: signal 0 only probes for existence.
            unsafe { libc::kill(pid, 0) == -1 }
        });
        assert!(gone, "the grandchild outlived a normal exit");
    }

    #[test]
    fn python_grader_runs_in_the_workdir_when_python3_exists() {
        if !crate::commands::ctx::adapters::program_is_present("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.py"), "VALUE = 3\n").unwrap();
        let pass = task(
            "[[task.grader]]\ntype = \"python\"\ncode = \"import sys; sys.path.insert(0, '.')\\nimport m\\nassert m.VALUE == 3\"\n",
        );
        assert_eq!(graded(&pass, "", dir.path(), true).passed, 1);
        let fail = task("[[task.grader]]\ntype = \"python\"\ncode = \"raise SystemExit(1)\"\n");
        assert_eq!(
            graded(&fail, "", dir.path(), true),
            Grade {
                passed: 0,
                evaluated: 1
            }
        );
    }

    #[test]
    fn parse_rejects_duplicate_ids_graderless_tasks_and_bad_schema() {
        let dup = "schema = 1\n[[task]]\nid = \"a\"\nrole = \"worker\"\ncomplexity = \"bounded\"\nprompt = \"p\"\n[[task.grader]]\ntype = \"answer_regex\"\npattern = 'x'\n[[task]]\nid = \"a\"\nrole = \"worker\"\ncomplexity = \"bounded\"\nprompt = \"p\"\n[[task.grader]]\ntype = \"answer_regex\"\npattern = 'x'\n";
        assert!(parse(dup).unwrap_err().contains("duplicate"));
        let bare = "schema = 1\n[[task]]\nid = \"a\"\nrole = \"worker\"\ncomplexity = \"bounded\"\nprompt = \"p\"\n";
        assert!(parse(bare).unwrap_err().contains("no grader"));
        assert!(parse("schema = 2\n").unwrap_err().contains("schema"));
        let bad = "schema = 1\n[[task]]\nid = \"a\"\nrole = \"worker\"\ncomplexity = \"bounded\"\nprompt = \"p\"\n[[task.grader]]\ntype = \"answer_regex\"\npattern = '('\n";
        assert!(parse(bad).is_err());
    }

    #[test]
    fn the_embedded_corpus_is_consistent() {
        let corpus = embedded().expect("embedded corpus parses");
        assert_eq!(corpus.tasks.len(), 13);
        let mut roles = BTreeSet::new();
        for task in &corpus.tasks {
            roles.insert(task.role);
        }
        assert_eq!(roles.len(), 2);
        let files: BTreeSet<&str> = FIXTURE.iter().map(|(path, _)| *path).collect();
        for task in &corpus.tasks {
            for grader in &task.graders {
                if let Grader::FileRegex { path, .. } = grader {
                    assert!(files.contains(path.as_str()), "{}: {path}", task.id);
                }
            }
        }
    }

    #[test]
    fn a_task_without_a_complexity_is_rejected() {
        let text = "schema = 1\n[[task]]\nid = \"a\"\nrole = \"worker\"\nprompt = \"p\"\n[[task.grader]]\ntype = \"answer_regex\"\npattern = 'x'\n";
        assert!(parse(text).unwrap_err().contains("complexity"));
    }

    #[test]
    fn a_task_routes_as_its_role_unless_it_names_a_route_role() {
        let corpus = embedded().expect("embedded corpus parses");
        let role_of = |id: &str| {
            corpus
                .tasks
                .iter()
                .find(|task| task.id == id)
                .map(Task::routing_role)
        };
        assert_eq!(role_of("w-read"), Some(RouteRole::Worker));
        assert_eq!(role_of("o-plan"), Some(RouteRole::Orchestrator));
        assert_eq!(role_of("o-review"), Some(RouteRole::Reviewer));
    }

    #[test]
    fn every_class_the_router_uses_has_at_least_two_tasks() {
        let corpus = embedded().expect("embedded corpus parses");
        let count = |role: RouteRole, complexity: CellComplexity| {
            corpus
                .tasks
                .iter()
                .filter(|task| task.routing_role() == role && task.complexity == complexity)
                .count()
        };
        for (role, complexity) in [
            (RouteRole::Worker, CellComplexity::Trivial),
            (RouteRole::Worker, CellComplexity::Bounded),
            (RouteRole::Worker, CellComplexity::Substantial),
            (RouteRole::Reviewer, CellComplexity::Bounded),
            (RouteRole::Orchestrator, CellComplexity::Bounded),
            (RouteRole::Orchestrator, CellComplexity::Substantial),
        ] {
            assert!(count(role, complexity) >= 2, "{role:?} {complexity:?}");
        }
    }

    #[test]
    fn materialize_writes_every_fixture_file() {
        let dir = tempfile::tempdir().unwrap();
        materialize(dir.path()).unwrap();
        for (relative, contents) in FIXTURE {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(relative)).unwrap(),
                contents
            );
        }
    }
}
