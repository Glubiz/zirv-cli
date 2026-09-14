//! ZCHK-NATIVE-PARITY (issue #492, N23): `docs/design/native-parity.md` is
//! the release-blocking parity matrix -- one row per capability the runtime
//! inventory knows about, each naming the native path, the legacy path, the
//! provider/platform requirement, the evidence and the strength of the claim
//! (its *rung*). A matrix like that is only worth reading if it cannot
//! quietly claim more than it has, so this check reads it against
//! `native-runtime-inventory.md` and the real tree on every run and fails
//! when the two disagree or when a row over-claims:
//!
//! - an inventory capability (a clap verb, or a model-calling call site) has
//!   no parity row, or a parity row names a capability the inventory does
//!   not have;
//! - a row cites a test that does not exist *in the module it names* (the
//!   citation's module path is resolved to a real file under `src/`, and the
//!   `fn` has to be in THAT file -- a bare name that happens to exist
//!   somewhere else in the tree is not evidence for this row), a CI step that
//!   is not a `- name:` step in `.github/workflows/ci.yaml`, or a
//!   `docs/benchmarks/` file that is not committed;
//! - a row claims the `live-validated` rung without a recorded evidence file
//!   under `docs/benchmarks/`;
//! - a row that is not `legacy-only` carries no evidence at all and is not
//!   named under the document's own "Release blockers" heading;
//! - a `legacy-only` row does not say, in its `Requires` cell, why there is
//!   no native path.
//!
//! It is a sibling of [`super::inventory`] rather than more logic inside it:
//! that check answers "does every verb and call site have an OWNER", this one
//! answers "does every one of them have PROVEN parity", and the two fail for
//! different reasons with different fixes. The table/backtick/path-safety
//! parsing is reused from that module, not reimplemented.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::BuiltinCheckResult;
use super::inventory::{
    COMMANDS_HEADER_ROW, COMMANDS_HEADING, ENTRY_POINTS_HEADER_ROW, ENTRY_POINTS_HEADING,
    INVENTORY_PATH, backticked, extract_table, is_repo_relative,
};

pub const ID: &str = "ZCHK-NATIVE-PARITY";
const PROVES: &str = "every capability in docs/design/native-runtime-inventory.md has a row in \
     docs/design/native-parity.md whose evidence really exists in the tree, and no row claims a \
     rung its evidence does not support";
const FIX: &str = "add or correct the row in docs/design/native-parity.md -- every inventory verb \
     and every model-calling call site needs one row naming a real test as \
     `<module path>::tests::<fn>`, where the module path resolves to exactly one file under src/ \
     and that file declares the fn; or a real `CI: <step>` naming a `- name:` step in \
     .github/workflows/ci.yaml; or a committed docs/benchmarks/ file. `live-validated` needs the \
     benchmark pointer, `legacy-only` needs a reason in its Requires cell, and an evidence-free \
     row must be listed under `## Release blockers`";
const ORIGIN: &str = "issue #492 (N23): the roadmap may only close on an HONESTLY scored parity \
     record -- a matrix nothing checks is a claim, and the acceptance criterion is that no row \
     claims more than its evidence";

const PARITY_PATH: &str = "docs/design/native-parity.md";
const CI_PATH: &str = ".github/workflows/ci.yaml";
const BENCHMARK_PREFIX: &str = "docs/benchmarks/";
const BLOCKERS_HEADING: &str = "## Release blockers";
const COMMAND_MATRIX_HEADING: &str = "## Capability matrix: command verbs";
const CALL_SITE_MATRIX_HEADING: &str = "## Capability matrix: model-calling call sites";
const MATRIX_HEADER_ROW: &str =
    "| Capability | Native path | Legacy path | Requires | Evidence | Rung |";

/// The five rungs, weakest claim first. A row may name exactly one.
const RUNGS: &[&str] = &[
    "unit",
    "integration",
    "ci-matrix",
    "live-validated",
    "legacy-only",
];

pub fn run(repo: &Path) -> BuiltinCheckResult {
    if !super::is_zirv_repo(repo) {
        return BuiltinCheckResult::not_applicable(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            super::not_the_zirv_repo(repo),
        );
    }

    let inventory_path = repo.join(INVENTORY_PATH);
    let parity_path = repo.join(PARITY_PATH);
    let inventory = match std::fs::read_to_string(&inventory_path) {
        Ok(text) => text,
        Err(err) => {
            return inconclusive(format!("cannot read {}: {err}", inventory_path.display()));
        }
    };
    let parity = match std::fs::read_to_string(&parity_path) {
        Ok(text) => text,
        Err(err) => return inconclusive(format!("cannot read {}: {err}", parity_path.display())),
    };

    let Some(required) = required_capabilities(&inventory) else {
        return inconclusive(format!(
            "could not parse both inventory tables in {}",
            inventory_path.display()
        ));
    };
    let Some(rows) = matrix_rows(&parity) else {
        return inconclusive(format!(
            "could not find both `{COMMAND_MATRIX_HEADING}` and `{CALL_SITE_MATRIX_HEADING}` \
             tables in {}",
            parity_path.display()
        ));
    };

    let declared_blockers = release_blockers(&parity);
    let mut modules = SourceModules::scan(repo);
    let ci = ci_step_names(&std::fs::read_to_string(repo.join(CI_PATH)).unwrap_or_default());

    let mut problems = Vec::new();
    let mut documented: BTreeSet<String> = BTreeSet::new();
    let mut by_rung: BTreeMap<String, usize> = BTreeMap::new();
    let mut blockers: Vec<String> = Vec::new();

    for row in &rows {
        let Some(capability) = row.first().and_then(|cell| backticked(cell)) else {
            problems.push(format!("matrix row has no backticked capability: {row:?}"));
            continue;
        };
        let requires = row.get(3).map(String::as_str).unwrap_or("").trim();
        let evidence = row.get(4).map(String::as_str).unwrap_or("");
        let Some(rung) = row.get(5).and_then(|cell| backticked(cell)) else {
            problems.push(format!("`{capability}`: no backticked rung"));
            documented.insert(capability);
            continue;
        };
        if !RUNGS.contains(&rung.as_str()) {
            problems.push(format!(
                "`{capability}`: rung `{rung}` is not one of {}",
                RUNGS.join(" / ")
            ));
        }
        if !required.contains(&capability) {
            problems.push(format!(
                "`{capability}` has a parity row but is not a capability the inventory knows \
                 (stale row)"
            ));
        }
        documented.insert(capability.clone());
        *by_rung.entry(rung.clone()).or_default() += 1;

        let citations: Vec<String> = backticked_all(evidence);
        if citations.is_empty() {
            if rung == "legacy-only" {
                if requires.is_empty() || requires == "none" {
                    problems.push(format!(
                        "`{capability}`: a `legacy-only` row must say in its Requires cell WHY \
                         there is no native path"
                    ));
                }
            } else if declared_blockers.contains(&capability) {
                blockers.push(capability.clone());
            } else {
                problems.push(format!(
                    "`{capability}`: no evidence, and not listed under `{BLOCKERS_HEADING}` -- an \
                     evidence-free capability is a release blocker and must be named there"
                ));
                blockers.push(capability.clone());
            }
        }

        let mut has_benchmark = false;
        for citation in &citations {
            if let Some(step) = citation.strip_prefix("CI:") {
                let step = step.trim();
                if step.is_empty() || !ci.contains(step) {
                    problems.push(format!(
                        "`{capability}`: CI evidence `{step}` is not a `- name:` step in {CI_PATH}"
                    ));
                }
            } else if citation.contains('/') {
                if !citation.starts_with(BENCHMARK_PREFIX) {
                    problems.push(format!(
                        "`{capability}`: file evidence `{citation}` must live under \
                         {BENCHMARK_PREFIX}"
                    ));
                } else if !is_repo_relative(citation) || !repo.join(citation).is_file() {
                    problems.push(format!(
                        "`{capability}`: recorded evidence `{citation}` is not a committed file"
                    ));
                } else {
                    has_benchmark = true;
                }
            } else if let Err(problem) = modules.verify(citation) {
                problems.push(format!("`{capability}`: {problem}"));
            }
        }
        if rung == "live-validated" && !has_benchmark {
            problems.push(format!(
                "`{capability}`: the `live-validated` rung needs a recorded run committed under \
                 {BENCHMARK_PREFIX}"
            ));
        }
    }

    for capability in required.difference(&documented) {
        problems.push(format!(
            "`{capability}` is in the runtime inventory but has no parity row"
        ));
    }

    if problems.is_empty() {
        let counts: Vec<String> = by_rung
            .iter()
            .map(|(rung, count)| format!("{count} {rung}"))
            .collect();
        BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "{} capabilities carry a parity row with real evidence ({}); {} release blocker(s)",
                documented.len(),
                counts.join(", "),
                blockers.len()
            ),
        )
    } else {
        BuiltinCheckResult::fail(ID, PROVES, FIX, ORIGIN, problems.join("; "))
    }
}

fn inconclusive(details: String) -> BuiltinCheckResult {
    BuiltinCheckResult::inconclusive(ID, PROVES, FIX, ORIGIN, details)
}

/// Every capability key the inventory declares: a command verb as written,
/// and a model-calling call site as `path::symbol`.
fn required_capabilities(inventory: &str) -> Option<BTreeSet<String>> {
    let commands = extract_table(inventory, COMMANDS_HEADING, COMMANDS_HEADER_ROW)?;
    let entries = extract_table(inventory, ENTRY_POINTS_HEADING, ENTRY_POINTS_HEADER_ROW)?;
    let mut keys = BTreeSet::new();
    for row in commands {
        if let Some(verb) = row.first().and_then(|cell| backticked(cell)) {
            keys.insert(verb);
        }
    }
    for row in entries {
        let path = row.get(1).and_then(|cell| backticked(cell));
        let symbol = row.get(2).and_then(|cell| backticked(cell));
        if let (Some(path), Some(symbol)) = (path, symbol) {
            keys.insert(format!("{path}::{symbol}"));
        }
    }
    Some(keys)
}

/// Both matrix tables' data rows, concatenated.
fn matrix_rows(parity: &str) -> Option<Vec<Vec<String>>> {
    let mut rows = extract_table(parity, COMMAND_MATRIX_HEADING, MATRIX_HEADER_ROW)?;
    rows.extend(extract_table(
        parity,
        CALL_SITE_MATRIX_HEADING,
        MATRIX_HEADER_ROW,
    )?);
    Some(rows)
}

/// The capabilities named (backticked) under the document's own
/// `## Release blockers` heading, up to the next `## ` heading.
fn release_blockers(parity: &str) -> BTreeSet<String> {
    let Some(start) = parity.find(BLOCKERS_HEADING) else {
        return BTreeSet::new();
    };
    let section = &parity[start + BLOCKERS_HEADING.len()..];
    let end = section.find("\n## ").unwrap_or(section.len());
    backticked_all(&section[..end]).into_iter().collect()
}

/// Every `` `...` `` span in `cell`, in order.
fn backticked_all(cell: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cell;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        let span = after[..close].trim();
        if !span.is_empty() {
            out.push(span.to_string());
        }
        rest = &after[close + 1..];
    }
    out
}

/// The step names `.github/workflows/ci.yaml` declares, read as whole
/// `- name: <step>` lines rather than as substrings of the file.
///
/// Review round 1: matching a `CI:` citation against the raw YAML meant any
/// fragment of any line -- a job id, a `run:` word, a comment -- counted as
/// evidence that a CI step exists, which is the same over-claim the whole
/// check exists to prevent. A step is a list item, so only list items count.
fn ci_step_names(ci: &str) -> BTreeSet<String> {
    ci.lines()
        .filter_map(|line| line.trim().strip_prefix("- name:"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// The module path -> file map of the real source tree, plus a memoised set
/// of the `fn` names each resolved file declares.
///
/// Review round 1: checking a cited test by its bare final segment against
/// one flat set of every `fn` name under `src/` meant
/// `totally::fake::module::a_real_test` passed as long as SOME function
/// called `a_real_test` existed anywhere. A citation's module path is the
/// part that says *where the evidence is*, so it is resolved to an actual
/// file and the `fn` must be in that file.
struct SourceModules {
    repo: PathBuf,
    /// `(module path segments, repo-relative file path)`, one entry per
    /// `.rs` file under `src/`. `x/mod.rs` and `x.rs` both have module path
    /// `..::x`, which is exactly how Rust resolves them.
    modules: Vec<(Vec<String>, PathBuf)>,
    declared: BTreeMap<PathBuf, BTreeSet<String>>,
}

impl SourceModules {
    fn scan(repo: &Path) -> Self {
        let src = repo.join("src");
        let mut modules = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !path.extension().is_some_and(|ext| ext == "rs") {
                    continue;
                }
                let Ok(relative) = path.strip_prefix(repo) else {
                    continue;
                };
                let mut segments: Vec<String> = relative
                    .with_extension("")
                    .components()
                    .filter_map(|component| match component {
                        std::path::Component::Normal(name) => {
                            Some(name.to_string_lossy().into_owned())
                        }
                        _ => None,
                    })
                    .collect();
                // Drop the leading `src`, and `mod`/`main`/`lib` file stems,
                // so `src/commands/ctx/mod.rs` is `commands::ctx`.
                if segments.first().is_some_and(|first| first == "src") {
                    segments.remove(0);
                }
                if segments
                    .last()
                    .is_some_and(|last| matches!(last.as_str(), "mod" | "main" | "lib"))
                {
                    segments.pop();
                }
                modules.push((segments, relative.to_path_buf()));
            }
        }
        Self {
            repo: repo.to_path_buf(),
            modules,
            declared: BTreeMap::new(),
        }
    }

    /// `Ok(())` when `citation` resolves to exactly one file that declares
    /// the cited `fn`; otherwise the problem, naming the resolved path.
    fn verify(&mut self, citation: &str) -> Result<(), String> {
        let Some((module, name)) = split_citation(citation) else {
            return Err(format!(
                "cited test `{citation}` names no module -- cite it as \
                 `<module path>::tests::<fn>` so the evidence can be located"
            ));
        };
        let matches: Vec<PathBuf> = self
            .modules
            .iter()
            .filter(|(path, _)| ends_with_module(path, &module))
            .map(|(_, file)| file.clone())
            .collect();
        match matches.as_slice() {
            [] => Err(format!(
                "cited test `{citation}`: no file under src/ has the module path `{}`",
                module.join("::")
            )),
            [file] => {
                if self.declares(file, name) {
                    Ok(())
                } else {
                    Err(format!(
                        "cited test `{citation}`: {} declares no `fn {name}`",
                        file.display()
                    ))
                }
            }
            many => Err(format!(
                "cited test `{citation}`: the module path `{}` is ambiguous ({}) -- lengthen it",
                module.join("::"),
                many.iter()
                    .map(|file| file.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    fn declares(&mut self, file: &Path, name: &str) -> bool {
        if !self.declared.contains_key(file) {
            let text = std::fs::read_to_string(self.repo.join(file)).unwrap_or_default();
            self.declared
                .insert(file.to_path_buf(), declared_fn_names(&text));
        }
        self.declared
            .get(file)
            .is_some_and(|names| names.contains(name))
    }
}

/// Splits `commands::ctx::runtime::native::tests::foo` into the module path
/// `[commands, ctx, runtime, native]` and the fn name `foo`. A leading
/// `crate` and every trailing `tests` segment are dropped: `tests` is an
/// inline `#[cfg(test)]` module, not a file of its own.
fn split_citation(citation: &str) -> Option<(Vec<&str>, &str)> {
    let mut parts: Vec<&str> = citation
        .split("::")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let name = parts.pop()?;
    while parts.first().is_some_and(|first| *first == "crate") {
        parts.remove(0);
    }
    while parts.last().is_some_and(|last| *last == "tests") {
        parts.pop();
    }
    (!parts.is_empty()).then_some((parts, name))
}

/// Whether `path` ends with `module` -- so a citation may name as much or as
/// little of the module path as it takes to be unambiguous, and
/// `commands::` may be spelled or left off.
fn ends_with_module(path: &[String], module: &[&str]) -> bool {
    path.len() >= module.len()
        && path[path.len() - module.len()..]
            .iter()
            .zip(module)
            .all(|(have, want)| have == want)
}

/// Every `fn <name>` this file declares, at any visibility.
fn declared_fn_names(text: &str) -> BTreeSet<String> {
    const PREFIXES: &[&str] = &[
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "pub(super) fn ",
        "async fn ",
        "pub async fn ",
        "const fn ",
        "pub const fn ",
        "unsafe fn ",
    ];
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        for prefix in PREFIXES {
            let Some(rest) = trimmed.strip_prefix(prefix) else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                .collect();
            if !name.is_empty() {
                names.insert(name);
            }
            break;
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVENTORY: &str = "## Commands\n\n| Verb | Owner | Notes |\n|---|---|---|\n\
         | `ctx wrap` | harness-backend |  |\n| `verify` | shared |  |\n\n\
         ## Model-calling entry points\n\n\
         | Entry point | Path | Symbol | Owner | Notes |\n|---|---|---|---|---|\n\
         | Native helper call | `src/commands/ctx/helper.rs` | `run` | N15 (#484) |  |\n";

    fn matrix(rows: &str) -> String {
        format!(
            "{BLOCKERS_HEADING}\n\nnone.\n\n\
             {COMMAND_MATRIX_HEADING}\n\n{MATRIX_HEADER_ROW}\n|---|---|---|---|---|---|\n{rows}\n\
             {CALL_SITE_MATRIX_HEADING}\n\n{MATRIX_HEADER_ROW}\n|---|---|---|---|---|---|\n\
             | `src/commands/ctx/helper.rs::run` | native | none | a route for the role | \
             `helper::tests::a_real_test` | `unit` |\n"
        )
    }

    const PASSING_ROWS: &str = "| `ctx wrap` | -- | a vendor TUI | a native session has no PTY | \
         -- | `legacy-only` |\n\
         | `verify` | shared | shared | none | `checks::tests::a_real_test` | `unit` |\n";

    /// The fixture tree is a miniature of the real one, because the check
    /// now RESOLVES a citation's module path: `helper::tests::a_real_test`
    /// has to land on `src/commands/ctx/helper.rs` and
    /// `checks::tests::a_real_test` on `src/commands/workflow/checks/mod.rs`.
    /// `other.rs` exists so a test can cite a name that really does exist in
    /// the tree, from a module that does not declare it.
    fn fixture(rows: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        super::super::write_manifest(repo, "zirv");
        std::fs::create_dir_all(repo.join("docs/design")).expect("mkdir docs");
        std::fs::create_dir_all(repo.join("src/commands/ctx")).expect("mkdir ctx");
        std::fs::create_dir_all(repo.join("src/commands/workflow/checks")).expect("mkdir checks");
        std::fs::create_dir_all(repo.join(".github/workflows")).expect("mkdir ci");
        std::fs::write(repo.join(INVENTORY_PATH), INVENTORY).expect("inventory");
        std::fs::write(repo.join(PARITY_PATH), matrix(rows)).expect("parity");
        std::fs::write(
            repo.join("src/commands/ctx/helper.rs"),
            "fn run() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a_real_test() {}\n}\n",
        )
        .expect("helper");
        std::fs::write(
            repo.join("src/commands/workflow/checks/mod.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn a_real_test() {}\n}\n",
        )
        .expect("checks");
        std::fs::write(
            repo.join("src/commands/ctx/other.rs"),
            "fn something_else() {}\n",
        )
        .expect("other");
        std::fs::write(
            repo.join(CI_PATH),
            concat!(
                "jobs:\n",
                "  native-install-platforms:\n",
                "    name: Native Install (matrix)\n",
                "    steps:\n",
                "      - name: Native Setup And Doctor With No Harness Installed\n",
                "        run: zirv ctx doctor --json\n",
            ),
        )
        .expect("ci");
        dir
    }

    fn outcome(dir: &tempfile::TempDir) -> BuiltinCheckResult {
        run(dir.path())
    }

    #[test]
    fn a_complete_matrix_with_real_evidence_passes() {
        let dir = fixture(PASSING_ROWS);
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
        assert!(result.details.contains("3 capabilities"), "{result:?}");
    }

    #[test]
    fn a_capability_with_no_parity_row_fails() {
        let dir = fixture(
            "| `ctx wrap` | -- | a vendor TUI | a native session has no PTY | -- | `legacy-only` |\n",
        );
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("`verify`"), "{result:?}");
        assert!(result.details.contains("no parity row"), "{result:?}");
    }

    #[test]
    fn a_cited_test_that_does_not_exist_fails() {
        let dir = fixture(&PASSING_ROWS.replace("a_real_test", "a_test_nobody_wrote"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("a_test_nobody_wrote"), "{result:?}");
        assert!(result.details.contains("declares no `fn"), "{result:?}");
    }

    /// Review round 1, the reason the flat name set had to go: a citation is
    /// evidence about ONE module. A name that really is a test somewhere
    /// else in the tree (`ctx::helper`'s own `a_real_test`, here) proves
    /// nothing about the module the row points a reader at, and the failure
    /// must name the file that was actually resolved.
    #[test]
    fn a_real_test_name_under_the_wrong_module_path_fails() {
        let dir = fixture(&PASSING_ROWS.replace("checks::tests::", "other::tests::"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("other.rs"), "{result:?}");
        assert!(
            result.details.contains("declares no `fn a_real_test`"),
            "{result:?}"
        );
    }

    /// A module path that names no file at all is a different failure from a
    /// file that simply lacks the fn, and says so.
    #[test]
    fn a_citation_naming_no_module_at_all_fails() {
        let dir = fixture(&PASSING_ROWS.replace("checks::tests::", "totally::fake::module::"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("no file under src/"), "{result:?}");
    }

    /// The third resolution outcome, and the one that actually bit the real
    /// matrix: `native` names two different modules in this tree. Both
    /// fixture files DECLARE the cited fn, so a pass here would mean the
    /// check had silently picked one -- the collision has to be refused
    /// before the fn lookup, and the message has to name every candidate so
    /// the author knows how far to lengthen the path.
    #[test]
    fn an_ambiguous_module_suffix_fails_and_names_every_colliding_file() {
        let dir =
            fixture(&PASSING_ROWS.replace("checks::tests::a_real_test", "native::tests::foo"));
        let colliding = [
            ["src", "commands", "ctx", "runtime", "native.rs"],
            ["src", "commands", "ctx", "session", "native.rs"],
        ];
        for parts in colliding {
            let path: std::path::PathBuf = parts.iter().collect();
            std::fs::create_dir_all(dir.path().join(path.parent().expect("parent")))
                .expect("mkdir");
            std::fs::write(dir.path().join(&path), "fn foo() {}\n").expect("write module");
        }

        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("ambiguous"), "{result:?}");
        for parts in colliding {
            // Built through `PathBuf` so the assertion reads the same
            // separator the check's own `display()` wrote.
            let shown = parts.iter().collect::<std::path::PathBuf>();
            assert!(
                result.details.contains(&shown.display().to_string()),
                "{} must be named as a candidate: {result:?}",
                shown.display()
            );
        }
    }

    /// `tests` is an inline `#[cfg(test)]` module, never a file, so it is
    /// stripped -- which leaves a bare `tests::foo` citation with no module
    /// path at all. That is the "names no module" failure, not a lookup for
    /// a module called `tests`.
    #[test]
    fn a_citation_with_only_a_tests_segment_names_no_module_and_fails() {
        let dir = fixture(&PASSING_ROWS.replace("checks::tests::a_real_test", "tests::foo"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("names no module"), "{result:?}");
        assert!(
            result.details.contains("<module path>::tests::<fn>"),
            "{result:?}"
        );
    }

    /// The whole point of the rung vocabulary: `live-validated` is the one
    /// claim a fixture test can never earn, so it may only be written next to
    /// a recording that is actually committed.
    #[test]
    fn a_live_validated_claim_without_a_recording_fails() {
        let dir = fixture(&PASSING_ROWS.replace(
            "`checks::tests::a_real_test` | `unit`",
            "`checks::tests::a_real_test` | `live-validated`",
        ));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("live-validated"), "{result:?}");
        assert!(result.details.contains("docs/benchmarks/"), "{result:?}");
    }

    #[test]
    fn a_live_validated_claim_with_a_committed_recording_passes() {
        let dir = fixture(&PASSING_ROWS.replace(
            "`checks::tests::a_real_test` | `unit`",
            "`docs/benchmarks/run.md` | `live-validated`",
        ));
        std::fs::create_dir_all(dir.path().join("docs/benchmarks")).expect("mkdir");
        std::fs::write(dir.path().join("docs/benchmarks/run.md"), "recorded\n").expect("write");
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn an_evidence_free_row_that_is_not_a_declared_blocker_fails() {
        let dir =
            fixture(&PASSING_ROWS.replace("`checks::tests::a_real_test` | `unit`", "-- | `unit`"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("release blocker"), "{result:?}");
    }

    /// ...and the escape hatch is honest rather than silent: naming the
    /// capability under `## Release blockers` lets the build pass while the
    /// check reports it as a blocker in its own details.
    #[test]
    fn an_evidence_free_row_named_as_a_release_blocker_passes_and_is_counted() {
        let dir =
            fixture(&PASSING_ROWS.replace("`checks::tests::a_real_test` | `unit`", "-- | `unit`"));
        let parity = std::fs::read_to_string(dir.path().join(PARITY_PATH)).expect("parity");
        std::fs::write(
            dir.path().join(PARITY_PATH),
            parity.replace(
                "none.",
                "1. `verify` has no evidence yet; owner: this test.",
            ),
        )
        .expect("write");
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
        assert!(result.details.contains("1 release blocker"), "{result:?}");
    }

    #[test]
    fn a_legacy_only_row_with_no_reason_fails() {
        let dir = fixture(&PASSING_ROWS.replace("a native session has no PTY", "none"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("WHY there is no native"),
            "{result:?}"
        );
    }

    #[test]
    fn a_ci_citation_that_names_no_real_step_fails() {
        let dir = fixture(
            &PASSING_ROWS.replace("`checks::tests::a_real_test`", "`CI: A Job Nobody Wrote`"),
        );
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("A Job Nobody Wrote"), "{result:?}");
    }

    /// Review round 1: `CI:` evidence used to be a substring search over the
    /// whole YAML, so a prefix of a real step -- or a job id, or a word out
    /// of a `run:` block -- passed as proof that a step exists. Only whole
    /// `- name:` step lines count, so a shortened name fails even though
    /// every character of it is present in the file.
    #[test]
    fn a_partial_ci_step_name_fails() {
        let dir = fixture(&PASSING_ROWS.replace(
            "`checks::tests::a_real_test`",
            "`CI: Native Setup And Doctor`",
        ));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("is not a `- name:` step"),
            "{result:?}"
        );
    }

    /// ...and the whole step name still passes, so the rule narrows the
    /// match rather than breaking the citation form.
    #[test]
    fn a_whole_ci_step_name_passes() {
        let dir = fixture(&PASSING_ROWS.replace(
            "`checks::tests::a_real_test`",
            "`CI: Native Setup And Doctor With No Harness Installed`",
        ));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    /// The `Evidence` cell is repo-owned, UNTRUSTED text, and the check opens
    /// what it names: a file pointer that escapes `docs/benchmarks/` is
    /// refused rather than read.
    #[test]
    fn an_evidence_pointer_outside_the_benchmark_directory_fails() {
        let dir =
            fixture(&PASSING_ROWS.replace("`checks::tests::a_real_test`", "`../../etc/passwd`"));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("must live under"), "{result:?}");
    }

    #[test]
    fn a_row_for_a_capability_the_inventory_does_not_have_fails() {
        let dir = fixture(&format!(
            "{PASSING_ROWS}| `not-a-real-verb` | shared | shared | none | \
             `checks::tests::a_real_test` | `unit` |\n"
        ));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("stale row"), "{result:?}");
    }

    #[test]
    fn a_non_zirv_repo_is_not_applicable() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(dir.path(), "some-other-crate");
        let result = run(dir.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::NotApplicable,
            "{result:?}"
        );
    }

    #[test]
    fn a_missing_doc_inside_the_zirv_repo_is_inconclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::super::write_manifest(dir.path(), "zirv");
        let result = run(dir.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Inconclusive,
            "{result:?}"
        );
    }

    /// The matrix this repository actually ships must pass its own check --
    /// the release criterion is worth nothing otherwise.
    #[test]
    fn the_real_repo_parity_matrix_passes() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
