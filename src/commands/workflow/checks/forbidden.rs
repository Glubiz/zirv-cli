//! ZCHK-FORBIDDEN-WIDENING: every `ctx.toml` key enumerated by `config.rs`'s
//! `ENV_MAP` or `NON_ENV_CONFIG_SURFACES` tables is either repo-forbidden or
//! on this file's explicit narrow-only allow-list below. A brand new surface
//! landing without touching either classification fails this check --
//! today the untrusted-config posture ("repo-owned surfaces may only
//! NARROW", CLAUDE.md) is a checklist item a reviewer has to remember; this
//! makes it a check.
//!
//! `NON_ENV_CONFIG_SURFACES` is the mandatory companion enumeration for
//! list/table-shaped surfaces that cannot live in scalar `ENV_MAP`. The
//! workspace execution fields are the first entries: this prevents a future
//! executable surface from bypassing the gate merely because it has no env
//! override.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;

use super::BuiltinCheckResult;

pub const ID: &str = "ZCHK-FORBIDDEN-WIDENING";
const PROVES: &str = "every ctx.toml key config.rs explicitly enumerates is classified as \
     repo-forbidden or explicitly narrow-only (workflow::checks::forbidden::\
     NARROW_ONLY_ALLOWLIST)";
const FIX: &str = "classify the new key: add its dotted path to config.rs's REPO_FORBIDDEN (or \
     ARRAY_REPO_FORBIDDEN for array-of-table fields) -- operator-only is the default for \
     anything a repo checkout could widen -- or, if it can only narrow behavior, to \
     workflow::checks::forbidden::NARROW_ONLY_ALLOWLIST with a comment saying why";
const ORIGIN: &str = "untrusted-config posture (CLAUDE.md: repo-owned surfaces may only narrow) \
     was a checklist item, not a check -- issue #276";

/// Dotted `ctx.toml` key paths a repository checkout MAY set. Two different
/// reasons land a key here, and both are safe for the identical reason a
/// `REPO_FORBIDDEN` key is refused -- the repo cannot widen zirv's OWN
/// authority over the machine/process it did not already have:
///
/// - an explicit narrow-only fold (`config.rs`'s own doc comment on the
///   field documents the fold: a repo may disable a feature, lower a
///   ceiling, or tighten a stance, never the reverse); or
/// - a plain preference/timing/budget knob with no capability behind it at
///   all (how long to wait, how many entries to keep, how sensitive a
///   scoring threshold is) -- repo-settable by original design, not merely
///   overlooked.
///
/// Every entry below is reviewed alongside the PR that adds it; a new key
/// landing in `config.rs`'s `ENV_MAP` with neither an explicit narrow-only
/// fold nor a plain-preference justification belongs in `REPO_FORBIDDEN`
/// instead, not here.
pub const NARROW_ONLY_ALLOWLIST: &[&str] = &[
    // Narrow-only folds (config.rs's own doc comment on each field states
    // the fold in full):
    "workflow.deploy.minimum_tier", // folds as max(home, repo): repo may only RAISE its own floor.
    "supervise.orchestrator_writes", // repo may only tighten allow -> advise -> deny.
    "fallback.adaptive_delegation", // repo may only disable, per its own doc comment.
    "fallback.auto_orchestrator_rollover", // same AND-fold as adaptive_delegation.
    // Issue #455: the same AND-fold again -- a repo may switch the
    // route-health breaker off, never on for an operator who disabled it.
    // Its three timing knobs (`open_after_failures`, `window_secs`,
    // `cooldown_secs`) are `REPO_FORBIDDEN` outright.
    "fallback.health.enabled",
    // The scope-creep guard: a repo may switch it off, never on for an
    // operator who disabled it (`narrow_scope_guard_enabled`).
    "scope_guard.enabled",
    // Issue #466: a repo may request `mask` (narrower: strips the retained
    // domain hint), never force the operator's `mask` back to `keep`. See
    // config.rs's fold right beside `obfuscate.literals_file`'s own
    // `REPO_FORBIDDEN` row.
    "obfuscate.email_domain",
    // `chat.model` is deliberately not `REPO_FORBIDDEN` (see [[Untrusted
    // Configuration]] / README.md's own trust-boundary intro): the one model
    // key a repo may set at all, because a wrong model choice costs money,
    // not authority.
    "chat.model",
    // Plain preference/timing/budget knobs: no capability, no vendor
    // account, no filesystem/network/shell reach behind any of these --
    // adjusting them changes how zirv behaves for THIS repo, never what it
    // is allowed to do.
    "chrome.banner",
    "chrome.bar",
    "compact_advisory.min_reclaim_tokens",
    "compact_advisory.window_fraction",
    "dash.idle_quiet_ms",
    "fallback.enabled",
    "fallback.min_candidate_headroom_pct",
    "fallback.predictive_headroom_pct",
    "fallback.small_task_max_tokens",
    "fallback.small_task_max_tool_calls",
    "fallback.unknown_headroom_pct",
    "handoff.timeout_secs",
    // #406: a SCOPE knob on an advisory-only probe that never denies a
    // write -- listing a prefix can only make the reuse guard say less.
    "hooks.reuse_exclude",
    "mail.keep",
    "mail.max_message_bytes",
    "optimize.enabled",
    "optimize.sessions_sampled",
    "output.diff_max_bytes", // #412: repo may only lower the diff byte budget (min-merge).
    "pace.enabled",
    "pace.fallback_delay_secs",
    "pace.jitter_secs",
    "pace.max_percent",
    "pace.max_wait_secs",
    "pace.soft_percent",
    "pace.wait_slack_secs",
    "score.marker",
    "score.min_turns",
    "score.window",
    "supervise.interval_secs",
    "supervise.loop_backoff_ceiling_secs", // #311: repo may only lower the self-pacing ceiling (min-merge).
    "supervise.max_cycle_secs",
    "supervise.max_failures",
    "supervise.max_nudges",
    "supervise.max_restarts",
    "supervise.poll_ms",
    "worker.deny_network", // narrow-only fold: repo may only turn network OFF.
    "worker.max_depth",    // narrow-only fold: repo may only lower the depth cap.
    // Issue #718: min-fold like `worker.max_depth` -- a repo may only shrink
    // the warm worktree pool or expire it sooner.
    "worktree.idle_pool_max",
    "worktree.idle_ttl_secs",
    "wrap.debounce_ms",
    "wrap.inject_timeout_ms",
    // Issue #539 fix round: narrow-only fold (`narrow_skill_index_bool`) --
    // repo may only turn the standing skill index off, never force it back
    // on for an operator who disabled it.
    "prompt.skill_index",
    // #753: narrow-only fold (`narrow_intake_discipline_bool`) -- repo may
    // only turn the first-prompt discipline note off.
    "prompt.intake_discipline",
    // #716: these repository workspace fields are inert requirements. A name
    // only selects an entry; MCP names can only make launch validation refuse;
    // skills are labelled untrusted instructions. `workspace.git` and
    // `workspace.setup` are separately operator-only.
    "workspace.name",
    "workspace.mcp_servers",
    "workspace.skills",
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
    let path = repo.join("src/commands/ctx/config.rs");
    if !path.exists() {
        return BuiltinCheckResult::inconclusive(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            super::absent_input(&path),
        );
    }
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!("cannot read {}: {err}", path.display()),
            );
        }
    };

    let env_map_paths = match extract_table_paths(&source, "const ENV_MAP") {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "could not locate/parse config.rs's ENV_MAP table -- its shape changed \
                     since this check was written ({})",
                    path.display()
                ),
            );
        }
    };
    let non_env_paths = match extract_table_paths(&source, "const NON_ENV_CONFIG_SURFACES") {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "could not locate/parse config.rs's NON_ENV_CONFIG_SURFACES table -- its \
                     shape changed since this check was written ({})",
                    path.display()
                ),
            );
        }
    };
    let repo_forbidden_paths = match extract_table_paths(&source, "const REPO_FORBIDDEN") {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "could not locate/parse config.rs's REPO_FORBIDDEN table -- its shape \
                     changed since this check was written ({})",
                    path.display()
                ),
            );
        }
    };
    let array_forbidden_paths = match extract_table_paths(&source, "const ARRAY_REPO_FORBIDDEN") {
        Some(paths) if !paths.is_empty() => paths,
        _ => {
            return BuiltinCheckResult::inconclusive(
                ID,
                PROVES,
                FIX,
                ORIGIN,
                format!(
                    "could not locate/parse config.rs's ARRAY_REPO_FORBIDDEN table -- its \
                         shape changed since this check was written ({})",
                    path.display()
                ),
            );
        }
    };

    let forbidden_set: BTreeSet<&str> = repo_forbidden_paths
        .iter()
        .chain(array_forbidden_paths.iter())
        .map(String::as_str)
        .collect();
    let allow_set: BTreeSet<&str> = NARROW_ONLY_ALLOWLIST.iter().copied().collect();
    let all_paths: Vec<&String> = env_map_paths.iter().chain(non_env_paths.iter()).collect();

    let mut unclassified: Vec<String> = all_paths
        .iter()
        .filter(|path| {
            let path = path.as_str();
            !is_repo_forbidden(path, &forbidden_set) && !allow_set.contains(path)
        })
        .map(|path| path.as_str().to_string())
        .collect();
    unclassified.sort();
    unclassified.dedup();

    if unclassified.is_empty() {
        BuiltinCheckResult::pass(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "{} config keys checked ({} ENV_MAP, {} non-env): {} repo-forbidden, {} \
                 narrow-only allow-listed, 0 unclassified",
                all_paths.len(),
                env_map_paths.len(),
                non_env_paths.len(),
                all_paths
                    .iter()
                    .filter(|path| is_repo_forbidden(path.as_str(), &forbidden_set))
                    .count(),
                all_paths
                    .iter()
                    .filter(|path| allow_set.contains(path.as_str()))
                    .count(),
            ),
        )
    } else {
        BuiltinCheckResult::fail(
            ID,
            PROVES,
            FIX,
            ORIGIN,
            format!(
                "unclassified ctx.toml key(s), neither REPO_FORBIDDEN nor narrow-only \
                 allow-listed: {}",
                unclassified.join(", ")
            ),
        )
    }
}

/// Whether `path` (an `ENV_MAP` entry's dotted key, e.g. `"review.claude"`)
/// is covered by `forbidden` (the dotted `REPO_FORBIDDEN` paths) -- either
/// exactly, or because `REPO_FORBIDDEN` names an ancestor TABLE rather than
/// the leaf (`config.rs`'s own comment on `(&["review"], ...)`: "`value_at`
/// matches a table node the same way it matches a leaf ... this one entry
/// blocks both `review.claude` and `review.codex` together"). Component-wise
/// (splits on `.`), not a raw string prefix, so `"reviewer"` is never
/// wrongly covered by a `"review"` entry.
fn is_repo_forbidden(path: &str, forbidden: &BTreeSet<&str>) -> bool {
    if forbidden.contains(path) {
        return true;
    }
    let segments: Vec<&str> = path.split('.').collect();
    for prefix_len in 1..segments.len() {
        let prefix = segments[..prefix_len].join(".");
        if forbidden.contains(prefix.as_str()) {
            return true;
        }
    }
    false
}

/// Finds `const <name>: &[...] = &[ ... ];` in `source` and returns the
/// dotted path (`"score.window"`) for every `&["a", "b", ...]` bracketed
/// string-array literal found inside its body -- both `ENV_MAP` (whose path
/// array is the tuple's 2nd field) and `REPO_FORBIDDEN` (whose path array is
/// the tuple's 1st field) shape their entries this way, and neither table's
/// body contains any OTHER `&[...]` bracketed literal, so one pattern covers
/// both without needing to know which field position the path is in.
fn extract_table_paths(source: &str, const_marker: &str) -> Option<Vec<String>> {
    let start = source.find(const_marker)?;
    let assign = source[start..].find("= &[")? + start + 3; // position of the opening '['
    let mut depth = 0i32;
    let mut end = None;
    for (offset, ch) in source[assign..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(assign + offset + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &source[assign..end?];

    let bracket_re = Regex::new(r#"&\[\s*((?:"(?:[^"\\]|\\.)*"\s*,?\s*)*)\]"#).ok()?;
    let string_re = Regex::new(r#""((?:[^"\\]|\\.)*)""#).ok()?;

    let mut paths = Vec::new();
    for outer in bracket_re.captures_iter(body) {
        let inner = &outer[1];
        let segments: Vec<String> = string_re
            .captures_iter(inner)
            .map(|cap| cap[1].to_string())
            .collect();
        if !segments.is_empty() {
            paths.push(segments.join("."));
        }
    }
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_config_rs(repo: &Path, body: &str) {
        super::super::write_manifest(repo, "zirv");
        std::fs::create_dir_all(repo.join("src/commands/ctx")).unwrap();
        std::fs::write(repo.join("src/commands/ctx/config.rs"), body).unwrap();
    }

    /// Issue #406: the reuse probe's own scope knob is repo-settable, so it
    /// has to be classified here rather than in `REPO_FORBIDDEN`.
    #[test]
    fn the_allowlist_classifies_the_reuse_probe_scope_key() {
        assert!(
            NARROW_ONLY_ALLOWLIST.contains(&"hooks.reuse_exclude"),
            "got {NARROW_ONLY_ALLOWLIST:?}"
        );
    }

    #[test]
    fn another_repository_is_not_applicable() {
        let repo = tempdir().unwrap();
        super::super::write_manifest(repo.path(), "some-other-crate");
        let result = run(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::NotApplicable);
    }

    #[test]
    fn a_missing_config_rs_inside_the_zirv_repo_is_inconclusive() {
        let repo = tempdir().unwrap();
        super::super::write_manifest(repo.path(), "zirv");
        let result = run(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    /// A `config.rs` that EXISTS but whose tables this check can no longer
    /// parse is a degraded gate, not an inapplicable one -- issue #268's ban
    /// still applies to that case.
    #[test]
    fn an_unparseable_config_rs_stays_inconclusive() {
        let repo = tempdir().unwrap();
        write_config_rs(repo.path(), "// no ENV_MAP table here at all\n");
        let result = run(repo.path());
        assert_eq!(result.outcome, super::super::BuiltinOutcome::Inconclusive);
    }

    #[test]
    fn a_new_env_map_key_with_no_classification_fails() {
        let repo = tempdir().unwrap();
        write_config_rs(
            repo.path(),
            r#"
const ENV_MAP: &[(&str, &[&str], u8)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], 0),
    ("ZIRV_CTX_NEW_WIDENING_KEY", &["new", "widening_key"], 0),
];

const NON_ENV_CONFIG_SURFACES: &[&[&str]] = &[
    &["workspace", "git"],
];

const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent"], "ZIRV_CTX_AGENT"),
];

const ARRAY_REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["workspace", "git"], "operator only"),
];
"#,
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(result.details.contains("new.widening_key"), "{result:?}");
    }

    #[test]
    fn every_env_map_key_classified_passes() {
        let repo = tempdir().unwrap();
        write_config_rs(
            repo.path(),
            r#"
const ENV_MAP: &[(&str, &[&str], u8)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], 0),
    ("ZIRV_CTX_CHAT_MODEL", &["chat", "model"], 0),
];

const NON_ENV_CONFIG_SURFACES: &[&[&str]] = &[
    &["workspace", "git"],
];

const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent"], "ZIRV_CTX_AGENT"),
];

const ARRAY_REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["workspace", "git"], "operator only"),
];
"#,
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }

    #[test]
    fn a_new_non_env_surface_with_no_classification_fails() {
        let repo = tempdir().unwrap();
        write_config_rs(
            repo.path(),
            r#"
const ENV_MAP: &[(&str, &[&str], u8)] = &[
    ("ZIRV_CTX_AGENT", &["agent"], 0),
];

const NON_ENV_CONFIG_SURFACES: &[&[&str]] = &[
    &["workspace", "git"],
    &["workspace", "future_exec"],
];

const REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["agent"], "ZIRV_CTX_AGENT"),
];

const ARRAY_REPO_FORBIDDEN: &[(&[&str], &str)] = &[
    (&["workspace", "git"], "operator only"),
];
"#,
        );
        let result = run(repo.path());
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Fail,
            "{result:?}"
        );
        assert!(
            result.details.contains("workspace.future_exec"),
            "{result:?}"
        );
    }

    #[test]
    fn the_real_repo_config_rs_has_no_unclassified_keys() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = run(repo);
        assert_eq!(
            result.outcome,
            super::super::BuiltinOutcome::Pass,
            "{result:?}"
        );
    }
}
