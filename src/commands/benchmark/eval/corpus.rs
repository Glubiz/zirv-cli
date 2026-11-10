//! The embedded behaviour-eval cases, surface resolution and `lint`.
//!
//! A case names its surface (`layer:<role>`, `skill:<id>`, `mcp:<tool>`, `safety:<rule>`) and
//! never copies its text; the text is resolved from current source on every use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::grade::{Expect, grade};
use super::trace::trace;
use crate::commands::ctx::adapters;
use crate::commands::ctx::compile;
use crate::commands::ctx::config::EnvLookup;
use crate::commands::ctx::memory::sha256_hex;
use crate::commands::ctx::prompt::PromptRole;
use crate::commands::workflow::skill::SkillRegistry;

pub const DEFAULT_HARNESS: &str = "claude";

macro_rules! case_file {
    ($name:literal) => {
        include_str!(concat!("cases/", $name, ".toml"))
    };
}

macro_rules! canned_file {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../../../../tests/fixtures/eval/", $name)),
        )
    };
}

const CASES: [&str; 10] = [
    case_file!("skill-load-via-shell"),
    case_file!("skill-load-trivial-fix"),
    case_file!("no-unprompted-spend"),
    case_file!("no-unprompted-spend-explicit"),
    case_file!("ci-checks-before-done"),
    case_file!("ci-checks-doc-only"),
    case_file!("branch-before-push"),
    case_file!("branch-before-push-feature-branch"),
    case_file!("recall-before-substantive-work"),
    case_file!("recall-trivial-question"),
];

const CANNED: [(&str, &str); 20] = [
    canned_file!("skill-load-via-shell.pass.jsonl"),
    canned_file!("skill-load-via-shell.fail.jsonl"),
    canned_file!("skill-load-trivial-fix.pass.jsonl"),
    canned_file!("skill-load-trivial-fix.fail.jsonl"),
    canned_file!("no-unprompted-spend.pass.jsonl"),
    canned_file!("no-unprompted-spend.fail.jsonl"),
    canned_file!("no-unprompted-spend-explicit.pass.jsonl"),
    canned_file!("no-unprompted-spend-explicit.fail.jsonl"),
    canned_file!("ci-checks-before-done.pass.jsonl"),
    canned_file!("ci-checks-before-done.fail.jsonl"),
    canned_file!("ci-checks-doc-only.pass.jsonl"),
    canned_file!("ci-checks-doc-only.fail.jsonl"),
    canned_file!("branch-before-push.pass.jsonl"),
    canned_file!("branch-before-push.fail.jsonl"),
    canned_file!("branch-before-push-feature-branch.pass.jsonl"),
    canned_file!("branch-before-push-feature-branch.fail.jsonl"),
    canned_file!("recall-before-substantive-work.pass.jsonl"),
    canned_file!("recall-before-substantive-work.fail.jsonl"),
    canned_file!("recall-trivial-question.pass.jsonl"),
    canned_file!("recall-trivial-question.fail.jsonl"),
];

/// `bad` replays a recorded failure, `guard` protects a rule no incident backs yet, and
/// `control` is the same situation where the rule must not trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Bad,
    Guard,
    Control,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Setup {
    pub files: BTreeMap<String, String>,
    pub prior_turns: Vec<String>,
}

/// Fixture names (under `tests/fixtures/eval/`) of the canned traces that prove the grader.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canned {
    pub pass: String,
    #[serde(default)]
    pub fail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub kind: Kind,
    pub experience: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub control_of: Option<String>,
    pub surface: String,
    pub role: String,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub setup: Setup,
    pub turns: Vec<String>,
    pub expect: Expect,
    pub canned: Canned,
}

impl Case {
    pub fn harness(&self) -> &str {
        self.harness.as_deref().unwrap_or(DEFAULT_HARNESS)
    }
}

pub fn parse_case(text: &str) -> Result<Case, String> {
    let case: Case = toml::from_str(text).map_err(|error| format!("eval case: {error}"))?;
    if PromptRole::from_label(&case.role).is_none() {
        return Err(format!(
            "eval case '{}': unknown role '{}'",
            case.id, case.role
        ));
    }
    if case.turns.is_empty() {
        return Err(format!("eval case '{}': no turns", case.id));
    }
    if !matches!(case.harness(), "claude" | "codex") {
        return Err(format!(
            "eval case '{}': unknown harness '{}'",
            case.id,
            case.harness()
        ));
    }
    Ok(case)
}

pub fn embedded() -> Result<Vec<Case>, String> {
    let mut seen = BTreeSet::new();
    let mut cases = Vec::new();
    for text in CASES {
        let case = parse_case(text)?;
        if !seen.insert(case.id.clone()) {
            return Err(format!("eval corpus: duplicate case id '{}'", case.id));
        }
        cases.push(case);
    }
    Ok(cases)
}

pub fn canned_trace(name: &str) -> Option<&'static str> {
    CANNED
        .iter()
        .find(|(fixture, _)| *fixture == name)
        .map(|(_, text)| *text)
}

/// One surface resolved from current source.
#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub surface: String,
    pub text: String,
    /// sha256 over the surface reference and its text, so a baseline comparison proves the text changed.
    pub bundle_sha256: String,
    /// Per-surface size: the compile measure table for a layer, one row otherwise.
    pub context: String,
}

fn measure_row(label: &str, text: &str) -> String {
    let bytes = text.len();
    format!(
        "{label:<26} {bytes:>7} {:>8}\n~tokens = bytes / 4 (estimate)",
        compile::estimate_tokens(bytes)
    )
}

pub fn resolve_surface(
    surface: &str,
    harness: &str,
    repo: &Path,
    env: EnvLookup<'_>,
) -> Result<Resolved, String> {
    let (scheme, name) = surface
        .split_once(':')
        .filter(|(_, name)| !name.is_empty())
        .ok_or_else(|| format!("surface '{surface}': expected <scheme>:<name>"))?;
    let (text, context) = match scheme {
        "layer" => {
            let compiled = compile::compile_for_role(repo, env, Some(harness), Some(name))
                .map_err(|e| format!("surface '{surface}': {e}"))?;
            let text = compiled
                .compiled
                .composed
                .as_ref()
                .map(|c| c.text.clone())
                .ok_or_else(|| format!("surface '{surface}': prompt disabled by config"))?;
            let table =
                compile::render_measure_table(&compiled.compiled, &compiled.cfg, compiled.role);
            (text, table)
        }
        "skill" => {
            let home = crate::utils::home_dir().ok();
            let registry = SkillRegistry::load_for_repo(repo, home.as_deref(), true)
                .map_err(|e| format!("surface '{surface}': {e}"))?;
            let skill = registry
                .get(name)
                .map_err(|e| format!("surface '{surface}': {e}"))?;
            let text = format!(
                "{}\n\n{}",
                skill.manifest.description, skill.manifest.instructions
            );
            let context = measure_row(surface, &text);
            (text, context)
        }
        "mcp" => {
            let text = crate::commands::ctx::mcp::tool_description(name)
                .ok_or_else(|| format!("surface '{surface}': no such MCP tool"))?;
            let context = measure_row(surface, &text);
            (text, context)
        }
        "safety" => {
            let (verdict, reason) = adapters::SHIPPED_POSTURE_DENY
                .iter()
                .map(|entry| ("deny", entry))
                .chain(
                    adapters::SHIPPED_POSTURE_ASK
                        .iter()
                        .map(|entry| ("ask", entry)),
                )
                .find(|(_, (rule, _))| *rule == name)
                .map(|(verdict, (_, reason))| (verdict, *reason))
                .ok_or_else(|| format!("surface '{surface}': no such shipped safety rule"))?;
            let text = format!("{verdict}: {name} -- {reason}");
            let context = measure_row(surface, &text);
            (text, context)
        }
        other => return Err(format!("surface '{surface}': unknown scheme '{other}'")),
    };
    Ok(Resolved {
        surface: surface.to_string(),
        bundle_sha256: sha256_hex(&format!("{surface}\n{text}")),
        text,
        context,
    })
}

/// Every corpus problem, one line each; empty when the corpus is sound. `resolve` is called
/// once per distinct surface.
pub fn lint(
    cases: &[Case],
    canned: &dyn Fn(&str) -> Option<String>,
    resolve: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let ids: BTreeMap<&str, Kind> = cases.iter().map(|c| (c.id.as_str(), c.kind)).collect();
    let mut resolved = BTreeSet::new();
    for case in cases {
        let id = &case.id;
        if case.kind == Kind::Bad && case.source.as_deref().is_none_or(str::is_empty) {
            problems.push(format!("{id}: a bad case needs a source"));
        }
        match (case.kind, case.control_of.as_deref()) {
            (Kind::Control, None) => problems.push(format!("{id}: a control needs control_of")),
            (Kind::Control, Some(target))
                if ids.get(target).is_none_or(|k| *k == Kind::Control) =>
            {
                problems.push(format!(
                    "{id}: control_of '{target}' is not a bad or guard case"
                ));
            }
            (Kind::Bad | Kind::Guard, Some(_)) => {
                problems.push(format!("{id}: only a control may set control_of"));
            }
            _ => {}
        }
        if resolved.insert(case.surface.as_str())
            && let Err(error) = resolve(&case.surface)
        {
            problems.push(format!("{id}: {error}"));
        }
        if case.kind != Kind::Control && case.canned.fail.is_none() {
            problems.push(format!("{id}: needs a canned fail trace"));
        }
        let traces = [
            (Some(&case.canned.pass), true),
            (case.canned.fail.as_ref(), false),
        ];
        for (name, should_pass) in traces {
            let Some(name) = name else { continue };
            let want = if should_pass { "pass" } else { "fail" };
            let Some(text) = canned(name) else {
                problems.push(format!("{id}: canned {want} trace '{name}' is missing"));
                continue;
            };
            let graded = match trace(case.harness(), &text) {
                Ok(t) => grade(&case.expect, &t),
                Err(error) => {
                    problems.push(format!("{id}: canned {want} trace '{name}': {error}"));
                    continue;
                }
            };
            if graded.passed() != should_pass {
                let detail = graded
                    .checks
                    .iter()
                    .map(|c| format!("{}: {}", c.label, c.reason))
                    .collect::<Vec<_>>()
                    .join("; ");
                problems.push(format!(
                    "{id}: canned {want} trace '{name}' grades {}: {detail}",
                    if graded.passed() { "pass" } else { "fail" }
                ));
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::testenv;
    use std::collections::HashMap;

    fn load(id: &str) -> Case {
        embedded()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
    }

    fn canned_lookup(name: &str) -> Option<String> {
        canned_trace(name).map(str::to_string)
    }

    fn resolves(_: &str) -> Result<(), String> {
        Ok(())
    }

    const MINIMAL: &str = r#"
id = "c"
kind = "guard"
experience = "e"
surface = "mcp:skill_load"
role = "worker"
turns = ["t"]

[expect]
max_calls = 1

[canned]
pass = "p"
"#;

    #[test]
    fn the_seed_corpus_parses_with_five_cases_and_five_controls() {
        let cases = embedded().unwrap();
        assert_eq!(cases.len(), 10);
        let controls = cases.iter().filter(|c| c.kind == Kind::Control).count();
        assert_eq!(controls, 5);
        assert!(
            cases
                .iter()
                .filter(|c| c.kind == Kind::Bad)
                .all(|c| c.source.is_some())
        );
        for kind in [Kind::Bad, Kind::Guard] {
            assert!(cases.iter().any(|c| c.kind == kind));
        }
    }

    #[test]
    fn parse_reads_a_minimal_case() {
        let case = parse_case(MINIMAL).unwrap();
        assert_eq!(case.harness(), "claude");
        assert_eq!(case.expect.max_calls, Some(1));
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        assert!(parse_case(&format!("bogus = 1\n{MINIMAL}")).is_err());
        assert!(parse_case(&MINIMAL.replace("[expect]", "[expect]\nbogus = 1")).is_err());
        assert!(
            parse_case(&MINIMAL.replace("[canned]", "[setup]\nbogus = 1\n\n[canned]")).is_err()
        );
        assert!(parse_case(&MINIMAL.replace("pass = \"p\"", "pass = \"p\"\nbogus = 1")).is_err());
    }

    #[test]
    fn bad_role_harness_and_empty_turns_are_rejected() {
        assert!(parse_case(&MINIMAL.replace("worker", "boss")).is_err());
        assert!(parse_case(&MINIMAL.replace("turns = [\"t\"]", "turns = []")).is_err());
        assert!(parse_case(&MINIMAL.replace("role", "harness = \"goose\"\nrole")).is_err());
    }

    #[test]
    fn the_embedded_corpus_lints_clean_with_its_canned_traces() {
        let cases = embedded().unwrap();
        assert_eq!(
            lint(&cases, &canned_lookup, &resolves),
            Vec::<String>::new()
        );
    }

    #[test]
    fn lint_rejects_a_bad_case_without_source() {
        let mut case = load("skill-load-via-shell");
        case.source = None;
        let problems = lint(&[case], &canned_lookup, &resolves);
        assert!(
            problems.iter().any(|p| p.contains("needs a source")),
            "{problems:?}"
        );
    }

    #[test]
    fn lint_rejects_a_bad_case_whose_canned_fail_trace_passes() {
        let mut case = load("skill-load-via-shell");
        case.canned.fail = Some("skill-load-via-shell.pass.jsonl".to_string());
        let problems = lint(&[case], &canned_lookup, &resolves);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("canned fail trace") && p.contains("grades pass")),
            "{problems:?}"
        );
    }

    #[test]
    fn lint_rejects_a_pass_trace_that_fails_and_missing_traces() {
        let mut case = load("skill-load-via-shell");
        case.canned.pass = "skill-load-via-shell.fail.jsonl".to_string();
        case.canned.fail = Some("nope.jsonl".to_string());
        let problems = lint(&[case], &canned_lookup, &resolves);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("canned pass trace") && p.contains("grades fail"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("'nope.jsonl' is missing"))
        );
    }

    #[test]
    fn lint_requires_a_fail_trace_and_a_valid_control_target() {
        let mut guard = load("branch-before-push");
        guard.canned.fail = None;
        let mut control = load("branch-before-push-feature-branch");
        control.control_of = Some("ghost".to_string());
        let problems = lint(&[guard, control], &canned_lookup, &resolves);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("needs a canned fail trace"))
        );
        assert!(problems.iter().any(|p| p.contains("control_of 'ghost'")));
    }

    #[test]
    fn lint_reports_a_surface_that_does_not_resolve_once() {
        let cases = embedded().unwrap();
        let failing = |s: &str| -> Result<(), String> { Err(format!("cannot resolve {s}")) };
        let problems = lint(&cases, &canned_lookup, &failing);
        let layer = problems
            .iter()
            .filter(|p| p.contains("layer:orchestrator"))
            .count();
        assert_eq!(layer, 1, "{problems:?}");
    }

    #[test]
    fn every_seed_canned_trace_grades_as_expected() {
        for case in embedded().unwrap() {
            let pass = trace(case.harness(), canned_trace(&case.canned.pass).unwrap()).unwrap();
            assert!(
                grade(&case.expect, &pass).passed(),
                "{} pass trace",
                case.id
            );
            if let Some(fail) = &case.canned.fail {
                let fail = trace(case.harness(), canned_trace(fail).unwrap()).unwrap();
                assert!(
                    !grade(&case.expect, &fail).passed(),
                    "{} fail trace",
                    case.id
                );
            }
        }
    }

    #[test]
    fn layer_surfaces_resolve_for_the_case_harness_not_the_configured_agent() {
        let tmp = testenv::repo();
        let _home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let state = tmp.path().join("state").display().to_string();
        let plain: HashMap<&str, String> =
            [(crate::commands::ctx::state::STATE_ENV, state.clone())].into();
        let codex_configured: HashMap<&str, String> = [
            (crate::commands::ctx::state::STATE_ENV, state),
            ("ZIRV_CTX_AGENT", "codex".to_string()),
        ]
        .into();
        let _cwd = testenv::CwdGuard::enter(tmp.path()).unwrap();
        let resolve = |env: &HashMap<&str, String>, harness: &str| {
            resolve_surface("layer:worker", harness, tmp.path(), &|k: &str| {
                env.get(k).cloned()
            })
            .unwrap()
        };
        let claude = resolve(&plain, "claude");
        let pinned = resolve(&codex_configured, "claude");
        assert_eq!(pinned.bundle_sha256, claude.bundle_sha256);
        assert_eq!(pinned.text, claude.text);
        assert_ne!(
            resolve(&plain, "codex").bundle_sha256,
            claude.bundle_sha256,
            "the harness must change the compiled layer"
        );
    }

    #[test]
    fn surfaces_resolve_from_current_source() {
        let tmp = testenv::repo();
        let _home = testenv::HomeGuard::set(&tmp.path().join("home"));
        let env: HashMap<&str, String> = [(
            crate::commands::ctx::state::STATE_ENV,
            tmp.path().join("state").display().to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();
        let _cwd = testenv::CwdGuard::enter(tmp.path()).unwrap();
        for surface in [
            "layer:orchestrator",
            "layer:worker",
            "skill:implement",
            "mcp:skill_load",
            "safety:Bash(zirv benchmark run*)",
        ] {
            let resolved = resolve_surface(surface, "claude", tmp.path(), &lookup).unwrap();
            assert!(!resolved.text.is_empty(), "{surface}");
            assert_eq!(resolved.bundle_sha256.len(), 64, "{surface}");
            assert!(!resolved.context.is_empty(), "{surface}");
        }
        assert!(resolve_surface("layer:boss", "claude", tmp.path(), &lookup).is_err());
        assert!(resolve_surface("skill:no-such-skill", "claude", tmp.path(), &lookup).is_err());
        assert!(resolve_surface("mcp:no_such_tool", "claude", tmp.path(), &lookup).is_err());
        assert!(resolve_surface("safety:Bash(nope)", "claude", tmp.path(), &lookup).is_err());
        assert!(resolve_surface("nonsense", "claude", tmp.path(), &lookup).is_err());
        assert!(resolve_surface("web:x", "claude", tmp.path(), &lookup).is_err());
        let a = resolve_surface("mcp:skill_load", "claude", tmp.path(), &lookup).unwrap();
        let b = resolve_surface("mcp:skill_load", "claude", tmp.path(), &lookup).unwrap();
        assert_eq!(a.bundle_sha256, b.bundle_sha256);
    }
}
