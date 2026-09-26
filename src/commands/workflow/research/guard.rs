//! The trust boundary between a campaign manifest (repo-owned, untrusted
//! input authored to steer a candidate) and everything it is allowed to
//! touch: the compiled-in environment allowlist, source-patch scope, and
//! protected-evaluator hash drift detection. Every refusal here carries a
//! specific reason -- "not in the allowlist" and "fixed safety control" are
//! deliberately different messages, per issue #802.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::commands::ctx::CtxResult;

/// Env keys always refused, independent of the manifest's own `allow_env`:
/// the Jev gates that guard approval, injection screening, and required
/// verification, never candidate-tunable.
const FIXED_SAFETY_JEV_GATES: &[&str] = &[
    "APPROVE",
    "APPROVE_ALLOW",
    "INJECT_SCREEN",
    "STOP_VERIFY",
    "MISSING_TESTS",
    "REVIEW",
    "GATES",
    "ADMIN_DISPATCH",
];

/// Any key containing one of these (case-sensitive, matching the `ZIRV_CTX_*`
/// naming convention) is a fixed safety/verification/credential control,
/// whatever its exact name.
const FIXED_SAFETY_SUBSTRINGS: &[&str] =
    &["PERMISSION", "SANDBOX", "SAFETY", "CREDENTIAL", "BASE_URL"];

/// Jev boolean gates a candidate may tune (issue #803's tunable sites).
const JEV_BOOL_GATES: &[&str] = &[
    "MEMORY",
    "SUPERVISOR",
    "DISPATCH",
    "CONTEXT",
    "INTAKE_SAVINGS",
    "REVIEW_REUSE",
    "HARVEST_SCREEN",
    "CLASSIFY",
    "HANDOFF_SELECT",
    "COMPACTION_SELECT",
    "INJECT",
    "LAUNCH_EFFORT",
];

/// Sites with a tunable `min_confidence`/`min_margin` floor -- a subset of
/// `JEV_BOOL_GATES` (no floor for `supervisor`, `intake_savings`, or
/// `review_reuse`).
const JEV_FLOOR_SITES: &[&str] = &[
    "MEMORY",
    "CONTEXT",
    "HARVEST_SCREEN",
    "HANDOFF_SELECT",
    "COMPACTION_SELECT",
    "DISPATCH",
    "LAUNCH_EFFORT",
    "CLASSIFY",
    "INJECT",
];

/// The handover ladder's `<agent>_<tier>` axes.
const HANDOVER_AGENTS: &[&str] = &["CLAUDE", "CODEX"];
const HANDOVER_TIERS: &[&str] = &["CHEAP", "STANDARD", "DEEP"];

/// `headless.effort.*`'s accepted values, matching
/// `commands::ctx::config::CtxConfig`'s own validation (that match arm is
/// private to `config.rs`, so this list is a deliberate, commented
/// duplicate -- see the module doc comment on why `src/commands/ctx/` stays
/// read-only for this lane).
const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// `ZIRV_CTX_JEV_CACHE_TTL_SECS`'s declared ceiling (one week).
const MAX_CACHE_TTL_SECS: u64 = 604_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvValueKind {
    Bool,
    /// `[0, 1]` inclusive.
    Ratio01Closed,
    /// `(0, 1]`.
    Ratio01HalfOpen,
    TtlSecs,
    Model,
    Effort,
}

/// Classifies a candidate/cohort env key against the compiled-in allowlist.
/// `Err` carries a reason suitable for direct display: a fixed-safety
/// refusal names itself explicitly, everything else says it is not
/// allowlisted.
pub fn classify_env_key(key: &str) -> Result<EnvValueKind, String> {
    if FIXED_SAFETY_SUBSTRINGS
        .iter()
        .any(|needle| key.contains(needle))
    {
        return Err(format!(
            "'{key}' is a fixed safety/verification control and cannot be overlaid"
        ));
    }
    for gate in FIXED_SAFETY_JEV_GATES {
        if key == format!("ZIRV_CTX_JEV_{gate}") {
            return Err(format!(
                "'{key}' is a fixed safety/verification control and cannot be overlaid"
            ));
        }
    }
    for gate in JEV_BOOL_GATES {
        if key == format!("ZIRV_CTX_JEV_{gate}") {
            return Ok(EnvValueKind::Bool);
        }
    }
    for site in JEV_FLOOR_SITES {
        if key == format!("ZIRV_CTX_JEV_FLOOR_{site}_MIN_CONFIDENCE")
            || key == format!("ZIRV_CTX_JEV_FLOOR_{site}_MIN_MARGIN")
        {
            return Ok(EnvValueKind::Ratio01Closed);
        }
    }
    match key {
        "ZIRV_CTX_PROXY_MIN_CONFIDENCE" | "ZIRV_CTX_PROXY_MIN_MARGIN" => {
            return Ok(EnvValueKind::Ratio01Closed);
        }
        "ZIRV_CTX_SCORE_TOKEN_FLOOR_RATIO" | "ZIRV_CTX_SCORE_TOKEN_CEILING_RATIO" => {
            return Ok(EnvValueKind::Ratio01HalfOpen);
        }
        "ZIRV_CTX_JEV_CACHE_TTL_SECS" => return Ok(EnvValueKind::TtlSecs),
        "ZIRV_CTX_HEADLESS_EFFORT_TRIVIAL"
        | "ZIRV_CTX_HEADLESS_EFFORT_BOUNDED"
        | "ZIRV_CTX_HEADLESS_EFFORT_SUBSTANTIAL" => return Ok(EnvValueKind::Effort),
        _ => {}
    }
    for agent in HANDOVER_AGENTS {
        for tier in HANDOVER_TIERS {
            if key == format!("ZIRV_CTX_HANDOVER_{agent}_{tier}") {
                return Ok(EnvValueKind::Model);
            }
        }
    }
    Err(format!(
        "'{key}' is not in the compiled autoresearch env allowlist"
    ))
}

pub fn validate_env_value(
    kind: EnvValueKind,
    key: &str,
    value: &str,
    allowed_models: &[String],
) -> Result<(), String> {
    match kind {
        EnvValueKind::Bool => {
            if value != "true" && value != "false" {
                return Err(format!(
                    "'{key}' must be \"true\" or \"false\", got '{value}'"
                ));
            }
        }
        EnvValueKind::Ratio01Closed => {
            let parsed: f64 = value
                .parse()
                .map_err(|_| format!("'{key}' must be a number, got '{value}'"))?;
            if !(0.0..=1.0).contains(&parsed) {
                return Err(format!("'{key}' must be within [0, 1], got '{value}'"));
            }
        }
        EnvValueKind::Ratio01HalfOpen => {
            let parsed: f64 = value
                .parse()
                .map_err(|_| format!("'{key}' must be a number, got '{value}'"))?;
            if !(parsed > 0.0 && parsed <= 1.0) {
                return Err(format!("'{key}' must be within (0, 1], got '{value}'"));
            }
        }
        EnvValueKind::TtlSecs => {
            let parsed: u64 = value
                .parse()
                .map_err(|_| format!("'{key}' must be a non-negative integer, got '{value}'"))?;
            if parsed > MAX_CACHE_TTL_SECS {
                return Err(format!(
                    "'{key}' must be <= {MAX_CACHE_TTL_SECS} seconds, got '{value}'"
                ));
            }
        }
        EnvValueKind::Model => {
            if !allowed_models.iter().any(|m| m == value) {
                return Err(format!(
                    "'{key}' = '{value}' is not in candidate_space.allowed_models"
                ));
            }
        }
        EnvValueKind::Effort => {
            if !EFFORT_LEVELS.contains(&value) {
                return Err(format!(
                    "'{key}' must be one of {EFFORT_LEVELS:?}, got '{value}'"
                ));
            }
        }
    }
    Ok(())
}

/// A declared candidate's `env` overlay: every key must be in BOTH the
/// manifest's own `[candidate_space] allow_env` AND the compiled allowlist.
pub fn validate_candidate_env(
    env: &BTreeMap<String, String>,
    allow_env: &[String],
    allowed_models: &[String],
) -> Result<(), String> {
    for (key, value) in env {
        if !allow_env.iter().any(|allowed| allowed == key) {
            return Err(format!(
                "env key '{key}' is not in [candidate_space] allow_env"
            ));
        }
        let kind = classify_env_key(key)?;
        validate_env_value(kind, key, value, allowed_models)?;
    }
    Ok(())
}

/// `[cohort].env` (forced pressure applied to both arms): validated against
/// the same compiled allowlist, independent of the manifest's `allow_env`.
pub fn validate_cohort_env(
    env: &BTreeMap<String, String>,
    allowed_models: &[String],
) -> Result<(), String> {
    for (key, value) in env {
        let kind = classify_env_key(key)?;
        validate_env_value(kind, key, value, allowed_models)?;
    }
    Ok(())
}

/// Paths a source patch may never touch, whatever `allowed_paths` says.
pub const HARD_DENY_PATCH_PATTERNS: &[&str] = &[
    ".github/**",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    ".zirv/**",
    "src/settings.rs",
    "src/commands/ctx/safety.rs",
    "src/commands/ctx/price.rs",
];

/// `pattern` matches `path` exactly, or (for a `dir/**` pattern) `path` is
/// `dir` itself or lives under it. Both sides are normalized to forward
/// slashes first so a Windows-style numstat path still matches a Unix-style
/// glob.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    pattern == path
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchScopeViolation {
    pub path: String,
    pub reason: String,
}

/// Validates a source patch's touched paths (as `git apply --numstat` would
/// list them) against the manifest's `allowed_paths`, the compiled hard-deny
/// list, and the protected evaluator set. Pure -- takes plain path strings,
/// no filesystem or git access, so `parse_numstat`'s output can be checked
/// directly in a unit test.
pub fn validate_patch_scope(
    paths: &[String],
    allowed_paths: &[String],
    protected: &[String],
) -> Vec<PatchScopeViolation> {
    let mut violations = Vec::new();
    for path in paths {
        if HARD_DENY_PATCH_PATTERNS
            .iter()
            .any(|pattern| path_matches(pattern, path))
        {
            violations.push(PatchScopeViolation {
                path: path.clone(),
                reason: "path is hard-denied for every campaign".to_string(),
            });
            continue;
        }
        if protected.iter().any(|pattern| path_matches(pattern, path)) {
            violations.push(PatchScopeViolation {
                path: path.clone(),
                reason: "path is a protected evaluator file".to_string(),
            });
            continue;
        }
        if !allowed_paths
            .iter()
            .any(|pattern| path_matches(pattern, path))
        {
            violations.push(PatchScopeViolation {
                path: path.clone(),
                reason: "path is outside candidate_space.source_patch.allowed_paths".to_string(),
            });
        }
    }
    violations
}

/// Parses `git apply --numstat` output (`added\tdeleted\tpath` per line)
/// into the list of touched paths. A rename's `a/{old => new}/b` form is
/// left as-is -- scope validation still matches the braces literally against
/// `allowed_paths`, which is conservative (a rename inside an allowed
/// directory needs an explicit pattern) rather than silently permissive.
pub fn parse_numstat(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let _added = parts.next()?;
            let _deleted = parts.next()?;
            let path = parts.next()?.trim();
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Hashes one file's bytes, keyed by its path relative to `repo` (forward
/// slashes, so the fingerprint is stable across platforms).
pub fn hash_file(repo: &Path, relative: &Path) -> CtxResult<(String, String)> {
    let absolute = repo.join(relative);
    let bytes = std::fs::read(&absolute).map_err(|err| {
        format!(
            "could not read protected file '{}': {err}",
            absolute.display()
        )
    })?;
    let key = relative.to_string_lossy().replace('\\', "/");
    Ok((key, sha256_hex(&bytes)))
}

/// Hashes every path in `relatives` (already resolved, no globbing here) and
/// folds them into one campaign-wide fingerprint over the sorted
/// `path:hash` pairs.
pub fn hash_protected_files(
    repo: &Path,
    relatives: &[PathBuf],
) -> CtxResult<(BTreeMap<String, String>, String)> {
    let mut hashes = BTreeMap::new();
    for relative in relatives {
        let (key, hash) = hash_file(repo, relative)?;
        hashes.insert(key, hash);
    }
    let mut fingerprint_input = String::new();
    for (path, hash) in &hashes {
        fingerprint_input.push_str(path);
        fingerprint_input.push(':');
        fingerprint_input.push_str(hash);
        fingerprint_input.push('\n');
    }
    let fingerprint = sha256_hex(fingerprint_input.as_bytes());
    Ok((hashes, fingerprint))
}

/// Re-hashes `expected`'s own paths and reports every one that now differs
/// or is missing. An empty result means no drift.
pub fn detect_drift(repo: &Path, expected: &BTreeMap<String, String>) -> Vec<String> {
    let mut drifted = Vec::new();
    for (path, hash) in expected {
        let absolute = repo.join(path);
        match std::fs::read(&absolute) {
            Ok(bytes) => {
                if sha256_hex(&bytes) != *hash {
                    drifted.push(path.clone());
                }
            }
            Err(_) => drifted.push(path.clone()),
        }
    }
    drifted.sort();
    drifted
}

/// Compiled-in protected defaults, resolved relative to `benchmark_dir`
/// (the corpus file's parent directory) -- issue #801's evaluator set,
/// beyond whatever the manifest itself declares in `evaluator.protected`.
pub fn compiled_protected_defaults(benchmark_dir: &Path) -> Vec<PathBuf> {
    ["run.py", "grade.py", "quality_rubric.md"]
        .iter()
        .map(|name| benchmark_dir.join(name))
        .collect()
}

/// Expands a manifest-declared protected glob (`dir/**`, a single-level
/// `*`, or a literal path) against `repo` into concrete, existing file
/// paths relative to `repo`. Deliberately small: autoresearch manifests
/// name a handful of evaluator files, not an arbitrary tree.
pub fn expand_glob(repo: &Path, pattern: &str) -> Vec<PathBuf> {
    let pattern = pattern.replace('\\', "/");
    if let Some(dir) = pattern.strip_suffix("/**") {
        let root = repo.join(dir);
        let mut out = Vec::new();
        walk_files(&root, &mut out);
        return out
            .into_iter()
            .filter_map(|absolute| absolute.strip_prefix(repo).ok().map(PathBuf::from))
            .collect();
    }
    if let Some((dir, glob_name)) = pattern.rsplit_once('/')
        && let Some(prefix) = glob_name.strip_suffix('*')
    {
        let root = repo.join(dir);
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(prefix) && entry.path().is_file() {
                    out.push(PathBuf::from(dir).join(name.as_ref()));
                }
            }
        }
        return out;
    }
    let candidate = repo.join(&pattern);
    if candidate.is_file() {
        vec![PathBuf::from(pattern)]
    } else {
        Vec::new()
    }
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declared_bool_gate_accepts_true_and_false() {
        let kind = classify_env_key("ZIRV_CTX_JEV_MEMORY").expect("must be allowlisted");
        assert_eq!(kind, EnvValueKind::Bool);
        assert!(validate_env_value(kind, "ZIRV_CTX_JEV_MEMORY", "true", &[]).is_ok());
        assert!(validate_env_value(kind, "ZIRV_CTX_JEV_MEMORY", "false", &[]).is_ok());
        assert!(validate_env_value(kind, "ZIRV_CTX_JEV_MEMORY", "yes", &[]).is_err());
    }

    #[test]
    fn a_fixed_safety_gate_is_refused_with_its_own_reason() {
        let err = classify_env_key("ZIRV_CTX_JEV_APPROVE").expect_err("must be refused");
        assert!(
            err.contains("fixed safety/verification control"),
            "got: {err}"
        );
    }

    #[test]
    fn a_permission_key_is_refused_as_fixed_safety_even_if_never_named_explicitly() {
        let err = classify_env_key("ZIRV_CTX_SOME_PERMISSION_FLAG").expect_err("must be refused");
        assert!(
            err.contains("fixed safety/verification control"),
            "got: {err}"
        );
    }

    #[test]
    fn a_model_key_outside_allowed_models_is_refused() {
        let kind = classify_env_key("ZIRV_CTX_HANDOVER_CLAUDE_STANDARD").expect("allowlisted");
        let allowed = vec!["haiku".to_string(), "sonnet".to_string()];
        assert!(validate_env_value(kind, "k", "opus", &allowed).is_err());
        assert!(validate_env_value(kind, "k", "sonnet", &allowed).is_ok());
    }

    #[test]
    fn candidate_env_requires_both_allow_env_and_the_compiled_allowlist() {
        let mut env = BTreeMap::new();
        env.insert("ZIRV_CTX_JEV_MEMORY".to_string(), "true".to_string());
        // Not declared in allow_env.
        let err = validate_candidate_env(&env, &[], &[]).expect_err("must be refused");
        assert!(err.contains("allow_env"), "got: {err}");

        let allow_env = vec!["ZIRV_CTX_JEV_MEMORY".to_string()];
        assert!(validate_candidate_env(&env, &allow_env, &[]).is_ok());
    }

    #[test]
    fn cohort_env_is_validated_against_the_compiled_allowlist() {
        let mut env = BTreeMap::new();
        env.insert("ZIRV_CTX_JEV_APPROVE".to_string(), "false".to_string());
        let err = validate_cohort_env(&env, &[]).expect_err("must be refused");
        assert!(err.contains("fixed safety"), "got: {err}");

        let mut ok_env = BTreeMap::new();
        ok_env.insert(
            "ZIRV_CTX_PROXY_MIN_CONFIDENCE".to_string(),
            "0.5".to_string(),
        );
        assert!(validate_cohort_env(&ok_env, &[]).is_ok());
    }

    #[test]
    fn patch_scope_flags_a_protected_and_an_out_of_scope_path() {
        let numstat = "1\t1\tsrc/commands/ctx/safety.rs\n2\t0\tsrc/commands/ctx/proxy/decision.rs\n3\t0\tsrc/main.rs\n";
        let paths = parse_numstat(numstat);
        assert_eq!(
            paths,
            vec![
                "src/commands/ctx/safety.rs".to_string(),
                "src/commands/ctx/proxy/decision.rs".to_string(),
                "src/main.rs".to_string(),
            ]
        );

        let allowed = vec!["src/commands/ctx/proxy/decision.rs".to_string()];
        let violations = validate_patch_scope(&paths, &allowed, &[]);
        assert_eq!(violations.len(), 2);
        assert_eq!(violations[0].path, "src/commands/ctx/safety.rs");
        assert!(violations[0].reason.contains("hard-denied"));
        assert_eq!(violations[1].path, "src/main.rs");
        assert!(violations[1].reason.contains("outside"));
    }

    #[test]
    fn a_directory_glob_allows_every_file_beneath_it() {
        let allowed = vec!["src/commands/ctx/proxy/**".to_string()];
        let paths = vec!["src/commands/ctx/proxy/decision.rs".to_string()];
        assert!(validate_patch_scope(&paths, &allowed, &[]).is_empty());
    }

    #[test]
    fn drift_detection_notices_a_changed_byte() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("evaluator.py"), b"version-1").unwrap();
        let (hashes, _fingerprint) =
            hash_protected_files(repo.path(), &[PathBuf::from("evaluator.py")]).unwrap();
        assert!(detect_drift(repo.path(), &hashes).is_empty());

        std::fs::write(repo.path().join("evaluator.py"), b"version-2").unwrap();
        let drifted = detect_drift(repo.path(), &hashes);
        assert_eq!(drifted, vec!["evaluator.py".to_string()]);
    }

    #[test]
    fn drift_detection_notices_a_deleted_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("evaluator.py"), b"version-1").unwrap();
        let (hashes, _fingerprint) =
            hash_protected_files(repo.path(), &[PathBuf::from("evaluator.py")]).unwrap();

        std::fs::remove_file(repo.path().join("evaluator.py")).unwrap();
        let drifted = detect_drift(repo.path(), &hashes);
        assert_eq!(drifted, vec!["evaluator.py".to_string()]);
    }
}
