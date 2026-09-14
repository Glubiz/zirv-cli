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
//! - a row cites a test name that appears nowhere in `src/`, a CI step that
//!   appears nowhere in `.github/workflows/ci.yaml`, or a `docs/benchmarks/`
//!   file that is not committed;
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
use std::path::Path;

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
     and every model-calling call site needs one row naming a real test (its final `::` segment \
     must exist as an `fn` in src/), a real `CI: <step>` from .github/workflows/ci.yaml, or a \
     committed docs/benchmarks/ file; `live-validated` needs the benchmark pointer, \
     `legacy-only` needs a reason in its Requires cell, and an evidence-free row must be listed \
     under `## Release blockers`";
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
        Err(err) => return inconclusive(format!("cannot read {}: {err}", inventory_path.display())),
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
    let symbols = source_symbols(&repo.join("src"));
    let ci = std::fs::read_to_string(repo.join(CI_PATH)).unwrap_or_default();

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
                        "`{capability}`: CI evidence `{step}` does not appear in {CI_PATH}"
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
            } else {
                let name = citation.rsplit("::").next().unwrap_or(citation);
                if !symbols.contains(name) {
                    problems.push(format!(
                        "`{capability}`: cited test `{citation}` does not exist in src/"
                    ));
                }
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

/// Every `fn <name>` declared anywhere under `src/`. Read once per run: a
/// cited test name is checked against this set rather than by grepping the
/// tree per citation.
fn source_symbols(src: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                collect_fn_names(&text, &mut names);
            }
        }
    }
    names
}

fn collect_fn_names(text: &str, names: &mut BTreeSet<String>) {
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("fn ").or_else(|| {
            trimmed
                .strip_prefix("pub fn ")
                .or_else(|| trimmed.strip_prefix("async fn "))
        }) else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        if !name.is_empty() {
            names.insert(name);
        }
    }
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

    fn fixture(rows: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        super::super::write_manifest(repo, "zirv");
        std::fs::create_dir_all(repo.join("docs/design")).expect("mkdir docs");
        std::fs::create_dir_all(repo.join("src/commands")).expect("mkdir src");
        std::fs::create_dir_all(repo.join(".github/workflows")).expect("mkdir ci");
        std::fs::write(repo.join(INVENTORY_PATH), INVENTORY).expect("inventory");
        std::fs::write(repo.join(PARITY_PATH), matrix(rows)).expect("parity");
        std::fs::write(repo.join("src/commands/a.rs"), "fn a_real_test() {}\n").expect("src");
        std::fs::write(repo.join(CI_PATH), "name: Native Install\n").expect("ci");
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
        assert!(result.details.contains("does not exist"), "{result:?}");
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
        let dir = fixture(&PASSING_ROWS.replace("`checks::tests::a_real_test` | `unit`", "-- | `unit`"));
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
        let dir = fixture(&PASSING_ROWS.replace("`checks::tests::a_real_test` | `unit`", "-- | `unit`"));
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
        assert!(result.details.contains("WHY there is no native"), "{result:?}");
    }

    #[test]
    fn a_ci_citation_that_names_no_real_step_fails() {
        let dir = fixture(&PASSING_ROWS.replace(
            "`checks::tests::a_real_test`",
            "`CI: A Job Nobody Wrote`",
        ));
        let result = outcome(&dir);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("A Job Nobody Wrote"), "{result:?}");
    }

    /// The `Evidence` cell is repo-owned, UNTRUSTED text, and the check opens
    /// what it names: a file pointer that escapes `docs/benchmarks/` is
    /// refused rather than read.
    #[test]
    fn an_evidence_pointer_outside_the_benchmark_directory_fails() {
        let dir = fixture(&PASSING_ROWS.replace(
            "`checks::tests::a_real_test`",
            "`../../etc/passwd`",
        ));
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
