//! Evaluation rules for command safety.

use super::*;

/// Applies the SQL semantic analyzer to one executable candidate while
/// preserving explicit policy precedence. Keeping this rule in one helper is
/// what makes direct, compound, and shell-wrapped client invocations obey the
/// same narrowing/widening contract.
pub(super) fn apply_sql_outcome(policy: &SafetyPolicy, command: &str, base: Outcome) -> Outcome {
    if policy.sql == SqlMode::Off {
        return base;
    }
    let Some(sql) = sql_outcome(command) else {
        return base;
    };
    match (base.verdict, sql.verdict) {
        (Verdict::Deny, _) => base,
        (Verdict::Allow, Verdict::Ask) => sql,
        (_, Verdict::Allow) if base.matched.is_none() => sql,
        _ => base,
    }
}

pub(super) fn apply_credential_outcome(command: &str, base: Outcome) -> Outcome {
    if base.verdict == Verdict::Deny {
        return base;
    }
    let (verdict, pattern) = if is_sensitive_credential_access(command) {
        (Verdict::Deny, "<credential: sensitive-file access>")
    } else if is_sensitive_file_access(command, |path| project_secret_path(path, false)) {
        (Verdict::Ask, "<project secret file read>")
    } else {
        return base;
    };
    Outcome {
        verdict,
        matched: Some(Rule {
            pattern: pattern.to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

/// Deny writes to the operator `~/.zirv/` after ordinary rule evaluation:
/// broad allow rules must not bypass protection of the policy layer itself.
/// Reads and repo-local `.zirv/` writes remain unaffected.
pub(super) fn apply_operator_config_outcome(command: &str, base: Outcome) -> Outcome {
    if base.verdict == Verdict::Deny {
        return base;
    }
    if !writes_into_operator_zirv_config(command) {
        return if is_operator_config_edit(command) {
            Outcome {
                verdict: Verdict::Ask,
                matched: Some(Rule {
                    pattern: OPERATOR_CONFIG_EDIT_RULE.to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else {
            base
        };
    }
    Outcome {
        verdict: Verdict::Deny,
        matched: Some(Rule {
            pattern: "<config: operator ~/.zirv write>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

pub(super) const OPERATOR_CONFIG_EDIT_RULE: &str =
    "<config: operator ctx.toml edit via zirv ctx config>";

fn is_operator_config_edit(command: &str) -> bool {
    let Some(tokens) = sql_tokens(&collapse_whitespace(command)) else {
        return false;
    };
    tokens
        .first()
        .is_some_and(|s| sql_program_name(s) == "zirv")
        && tokens.get(1).is_some_and(|s| s.eq_ignore_ascii_case("ctx"))
        && tokens.get(2).is_some_and(|s| s == "config")
        && tokens
            .get(3)
            .is_some_and(|s| matches!(s.as_str(), "set" | "add"))
}

pub(super) fn operator_config_approval(outcome: &Outcome) -> bool {
    outcome.verdict == Verdict::Ask
        && outcome.matched.as_ref().is_some_and(|rule| {
            rule.origin == Origin::BuiltIn && rule.pattern == OPERATOR_CONFIG_EDIT_RULE
        })
}

pub(super) fn apply_network_outcome(command: &str, base: Outcome) -> Outcome {
    let Some(network) = network_outcome(command) else {
        return base;
    };
    if verdict_rank(network.verdict) > verdict_rank(base.verdict) {
        network
    } else {
        base
    }
}

/// Reconsider only the shipped recursive-delete Ask for targets provably
/// inside temp roots. Preserve operator/repo Ask, unrelated built-in Ask,
/// credential checks and every Deny; `original` supplies a leading `cd`.
pub(super) fn apply_recursive_delete_outcome(
    command: &str,
    original: &str,
    base: Outcome,
) -> Outcome {
    if !is_recursive_delete(command) {
        return base;
    }
    let overridable = base.verdict == Verdict::Allow
        || (base.verdict == Verdict::Ask
            && base.matched.as_ref().is_some_and(|rule| {
                rule.origin == Origin::BuiltIn
                    && matches!(rule.pattern.as_str(), "rm -rf *" | "rm -fr *")
            }));
    if overridable && recursive_delete_confined_to_temp(command, original) {
        return Outcome {
            verdict: Verdict::Allow,
            matched: Some(Rule {
                pattern: "<filesystem: temp-scratch recursive deletion>".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
    }
    if base.verdict != Verdict::Allow {
        return base;
    }
    Outcome {
        verdict: Verdict::Ask,
        matched: Some(Rule {
            pattern: "<filesystem: recursive deletion outside a generated directory>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

// Allow recursive deletion only when every target resolves lexically and
// strictly below a temp root; unresolved paths retain the shipped Ask.

/// The temp roots a recursive delete's targets may be confined to.
/// `std::env::temp_dir()` covers the platform default (and any `TMPDIR`/
/// `TEMP`/`TMP` override already baked into it); `/tmp` and `/var/tmp` are
/// included literally since a Bash command's own `/tmp` is a real, distinct
/// scratch path on every platform (including Windows Git Bash) regardless of
/// what `std::env::temp_dir()` itself reports for THIS process.
fn temp_delete_roots() -> Vec<String> {
    let mut roots = vec!["/tmp".to_string(), "/var/tmp".to_string()];
    let temp_dir = std::env::temp_dir()
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    if !temp_dir.is_empty() && !roots.contains(&temp_dir) {
        roots.push(temp_dir);
    }
    roots
}

/// Split an absolute Unix or Windows path into root and remainder; reject
/// relative paths rather than guessing the working directory.
fn split_absolute_root(path: &str) -> Option<(&str, &str)> {
    if let Some(rest) = path.strip_prefix('/') {
        return Some((&path[..1], rest));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'/' {
        return Some((&path[..3], &path[3..]));
    }
    None
}

/// Resolve path components lexically; reject `..` above the root because
/// its target cannot be proven from command text.
fn lexically_normalize_absolute(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let (root, rest) = split_absolute_root(&normalized)?;
    let mut stack: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    Some(format!("{root}{}", stack.join("/")))
}

/// True when `path` is STRICTLY below `root` (deleting `root` itself, e.g.
/// `rm -rf /tmp`, stays `Ask` -- the temp root is not itself a throwaway
/// scratch directory, and depending on the platform emptying it can break
/// other tools' own live state).
fn path_strictly_below(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    !root.is_empty() && root.len() != path.len() && path.starts_with(root) && {
        let rest = &path[root.len()..];
        rest.starts_with('/')
    }
}

/// Prove a recursive-delete target is strictly below a temp root. Dynamic
/// paths fail; relative targets require a known leading `cd` directory.
fn target_confined_to_temp(target: &str, cwd: Option<&str>) -> bool {
    let target = strip_quotes(target);
    if target.is_empty() || target.contains(['$', '`', '~', '*', '?']) {
        return false;
    }
    let normalized_target = target.replace('\\', "/");
    let absolute = if split_absolute_root(&normalized_target).is_some() {
        normalized_target
    } else {
        let Some(cwd) = cwd else { return false };
        if cwd.contains(['$', '`', '~', '*', '?']) || split_absolute_root(cwd).is_none() {
            return false;
        }
        format!("{}/{normalized_target}", cwd.trim_end_matches('/'))
    };
    let Some(resolved) = lexically_normalize_absolute(&absolute) else {
        return false;
    };
    temp_delete_roots()
        .iter()
        .any(|root| path_strictly_below(&resolved, root))
}

/// Extract only the delete command's targets, stopping at a shell chain
/// separator. Whole-command candidates may include following commands,
/// whose tokens must not be mistaken for delete targets and outvote a
/// confined delete segment in the worst-of-candidates fold.
fn delete_targets(command: &str) -> Option<Vec<String>> {
    let tokens = sql_tokens(&collapse_whitespace(command))?;
    let first = tokens.first()?;
    let program = normalized_delete_program(first);
    let mut targets = Vec::new();
    for token in tokens.iter().skip(1) {
        if matches!(token.as_str(), "&&" | "||" | ";" | "|") {
            break;
        }
        let is_flag = match program.as_str() {
            "rm" | "remove-item" => token.starts_with('-'),
            "rmdir" | "rd" | "del" | "erase" => token.starts_with('/') || token.starts_with('-'),
            _ => return None,
        };
        if !is_flag {
            targets.push(token.clone());
        }
    }
    Some(targets)
}

/// Parse a leading literal `cd <path>` with a following command; reject
/// dynamic or escaping paths before using it as a confinement root (#168).
pub(super) fn parse_leading_cd_segment(command: &str) -> Option<(String, String)> {
    let trimmed = command.trim_start();
    let rest = trimmed.strip_prefix("cd ")?;
    let (split_at, sep_len) = ["&&", ";", "\n"]
        .iter()
        .filter_map(|sep| rest.find(sep).map(|idx| (idx, sep.len())))
        .min_by_key(|&(idx, _)| idx)?;
    let (path_token, remainder) = {
        let (head, tail) = rest.split_at(split_at);
        (head.trim(), tail[sep_len..].trim())
    };
    if path_token.is_empty() || remainder.is_empty() {
        return None;
    }
    if path_token.split_whitespace().count() != 1
        || path_token.contains(['$', '`', '~', '*', '?'])
        || path_token.contains("..")
    {
        return None;
    }
    let normalized = strip_quotes(path_token).replace('\\', "/");
    Some((normalized, remainder.to_string()))
}

/// One unconfined target can delete outside scratch; require every target
/// below a temp root. Recover a leading `cd` from the original compound
/// because normalization removed it from the delete candidate.
fn recursive_delete_confined_to_temp(candidate: &str, original: &str) -> bool {
    let Some(targets) = delete_targets(candidate) else {
        return false;
    };
    if targets.is_empty() {
        return false;
    }
    let cwd = parse_leading_cd_segment(original).map(|(path, _)| path);
    targets
        .iter()
        .all(|target| target_confined_to_temp(target, cwd.as_deref()))
}

pub(super) fn apply_orchestrator_outcome(command: &str, base: Outcome) -> Outcome {
    if !is_destructive_orchestrator_action(command) || base.verdict != Verdict::Allow {
        return base;
    }
    Outcome {
        verdict: Verdict::Ask,
        matched: Some(Rule {
            pattern: "<orchestrator: destructive remote action>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

/// The POSIX shell interpreters a pipeline can be handed off to. Matched by
/// PROGRAM NAME (after path-stripping/case-normalization, the same as every
/// other classifier in this module), not by a literal substring of the
/// command text.
const SHELL_PIPE_TARGETS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// Wrapper/launcher programs whose own name is never the interesting part of
/// a pipe target -- each of these goes on to run some OTHER program, so
/// `curl x | env sh`, `| sudo sh`, `| timeout 5 sh` must still be read as
/// piping into `sh`, not compared against `env`/`sudo`/`timeout`. Matched by
/// program name, same normalization as everywhere else in this module.
const SHELL_PIPE_WRAPPER_PROGRAMS: &[&str] = &[
    "env",
    "exec",
    "command",
    "nohup",
    "sudo",
    "timeout",
    "stdbuf",
    "setsid",
    "xargs",
    "nice",
    "ionice",
    "time",
    "caffeinate",
    "busybox",
];

/// How many wrapper layers [`unwrap_pipe_wrapper`] will peel before giving up
/// -- small and finite so a deliberately long wrapper chain in an untrusted
/// command string cannot make this classifier do unbounded work.
const MAX_PIPE_WRAPPER_DEPTH: u8 = 8;

/// Unwrap known launchers to identify the executable in a pipeline stage.
/// Unknown syntax stops unwrapping so safety cannot be inferred from a guess.
fn unwrap_pipe_wrapper<'a>(tokens: &[&'a str], depth: u8) -> Option<&'a str> {
    let first = *tokens.first()?;
    let program = sql_program_name(first);
    if depth == 0 || !SHELL_PIPE_WRAPPER_PROGRAMS.contains(&program.as_str()) {
        return Some(first);
    }
    let mut i = 1usize;
    if program == "timeout" {
        while tokens.get(i).is_some_and(|t| t.starts_with('-')) {
            i += 1;
        }
        if tokens.get(i).is_some() {
            i += 1; // the mandatory DURATION positional.
        }
    }
    loop {
        match tokens.get(i) {
            Some(t) if t.starts_with('-') => {
                let consumes_value = LAUNCHER_PREFIXES
                    .iter()
                    .find(|entry| entry.program == program)
                    .is_some_and(|entry| entry.value_flags.contains(t));
                i += if consumes_value { 2 } else { 1 };
            }
            Some(t) if program == "env" && t.contains('=') => i += 1,
            _ => break,
        }
    }
    if i >= tokens.len() {
        return None;
    }
    unwrap_pipe_wrapper(&tokens[i..], depth - 1)
}

/// Splits `command` on a bare `|` (not `||`) while keeping quoted data
/// together -- the same quote-handling `split_segments` already applies,
/// narrowed to just the pipe operator so the caller can inspect the LAST
/// pipeline stage specifically. `;`/`&`/`&&`/`||` never introduce a
/// pipeline boundary here; they stay inside whatever stage they fall in.
pub(crate) fn pipeline_stages(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if escaped {
            current.push(c);
            escaped = false;
            i += 1;
        } else if c == '\\' && quote != Some('\'') {
            current.push(c);
            escaped = true;
            i += 1;
        } else if let Some(active) = quote {
            current.push(c);
            if c == active {
                quote = None;
            }
            i += 1;
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            current.push(c);
            i += 1;
        } else if c == '|' && next == Some('|') {
            current.push('|');
            current.push('|');
            i += 2;
        } else if c == '|' {
            stages.push(std::mem::take(&mut current));
            i += 1;
        } else {
            current.push(c);
            i += 1;
        }
    }
    stages.push(current);
    stages
}

/// Detect network fetches piped into shells across spacing and wrapper
/// variants. Unwrap env, launcher and `zirv ctx run --compact --` layers on
/// both ends, within a fixed depth, so wrappers cannot hide either program
/// from this compound-level Deny (#326).
fn unwrap_pipeline_stage_wrappers(stage: &str) -> String {
    let mut current = stage.to_string();
    for _ in 0..MAX_PIPE_WRAPPER_DEPTH {
        if let Some(inner) = unwrap_env_prefix(&current) {
            current = inner;
        } else if let Some(inner) = unwrap_launcher_prefix(&current) {
            current = inner;
        } else if let Some(inner) = unwrap_compact_run_wrapper(&current) {
            current = inner;
        } else {
            break;
        }
    }
    current
}

fn is_network_pipe_into_shell(command: &str) -> bool {
    let stages = pipeline_stages(command);
    if stages.len() < 2 {
        return false;
    }
    let Some(last) = stages.last() else {
        return false;
    };
    let last = unwrap_pipeline_stage_wrappers(last);
    let collapsed = collapse_whitespace(&last);
    let tokens: Vec<&str> = collapsed.split(' ').filter(|t| !t.is_empty()).collect();
    let Some(resolved) = unwrap_pipe_wrapper(&tokens, MAX_PIPE_WRAPPER_DEPTH) else {
        return false;
    };
    let program = sql_program_name(resolved);
    SHELL_PIPE_TARGETS.contains(&program.as_str())
        && stages[..stages.len() - 1]
            .iter()
            .any(|stage| is_network_fetching_stage(stage))
}

fn is_network_fetching_stage(stage: &str) -> bool {
    let stage = unwrap_pipeline_stage_wrappers(stage);
    let Some(tokens) = sql_tokens(&collapse_whitespace(&stage)) else {
        return false;
    };
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let Some(resolved) = unwrap_pipe_wrapper(&refs, MAX_PIPE_WRAPPER_DEPTH) else {
        return false;
    };
    matches!(
        sql_program_name(resolved).as_str(),
        "curl" | "wget" | "invoke-restmethod" | "invoke-webrequest" | "irm" | "iwr"
    )
}

pub(super) fn apply_pipe_to_shell_outcome(command: &str, base: Outcome) -> Outcome {
    if !is_network_pipe_into_shell(command) || base.verdict == Verdict::Deny {
        return base;
    }
    Outcome {
        verdict: Verdict::Deny,
        matched: Some(Rule {
            pattern: "<network: piped into a shell interpreter>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

/// `find -exec` programs admitted without Ask only if they cannot spawn
/// subprocesses or write via their own flags or language primitives.
/// `awk` and `sed` have execution/write primitives and cannot qualify.
/// A security decision, not a style choice: any future addition needs the
/// same audit spelled out here, not just "it looks like a reader".
const FIND_EXEC_SAFE_PROGRAMS: &[&str] = &[
    "grep",
    "egrep",
    "fgrep",
    "cat",
    "ls",
    "wc",
    "head",
    "tail",
    "stat",
    "file",
    "echo",
    "printf",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "du",
    "basename",
    "dirname",
    "test",
    "true",
    "false",
];

/// Ask for any `find -exec`/`-ok` action whose program is not proven safe;
/// unknown executables cannot inherit an ordinary read verdict.
fn is_risky_find_exec(command: &str) -> bool {
    let Some(tokens) = sql_tokens(&collapse_whitespace(command)) else {
        return false;
    };
    let Some(first) = tokens.first() else {
        return false;
    };
    if sql_program_name(first) != "find" {
        return false;
    }
    let mut i = 1;
    while i < tokens.len() {
        let lower = tokens[i].to_ascii_lowercase();
        if matches!(lower.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            match tokens.get(i + 1) {
                Some(action)
                    if FIND_EXEC_SAFE_PROGRAMS.contains(&sql_program_name(action).as_str()) => {}
                _ => return true,
            }
        }
        i += 1;
    }
    false
}

pub(super) fn apply_find_exec_outcome(command: &str, base: Outcome) -> Outcome {
    if !is_risky_find_exec(command) || base.verdict != Verdict::Allow {
        return base;
    }
    Outcome {
        verdict: Verdict::Ask,
        matched: Some(Rule {
            pattern: "<filesystem: find -exec/-ok running a non-read-only action>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

pub(super) fn apply_vcs_outcome(
    command: &str,
    base: Outcome,
    scratchpad_roots: &[String],
) -> Outcome {
    if !is_destructive_vcs_action(command, scratchpad_roots) || base.verdict != Verdict::Allow {
        return base;
    }
    Outcome {
        verdict: Verdict::Ask,
        matched: Some(Rule {
            pattern: "<vcs: destructive local or remote action>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

pub(super) fn apply_distribution_outcome(command: &str, base: Outcome) -> Outcome {
    if !is_irreversible_distribution_action(command) || base.verdict == Verdict::Deny {
        return base;
    }
    Outcome {
        verdict: Verdict::Deny,
        matched: Some(Rule {
            pattern: "<distribution: irreversible remote action>".to_string(),
            origin: Origin::BuiltIn,
        }),
    }
}

/// Evaluate raw and normalized candidates, keeping the strictest verdict.
/// SQL classification may narrow to Ask for unproven reads, or widen to Allow
/// only when no explicit rule matched; it never overrides Deny or a configured
/// Ask. Evaluation is pure.
pub fn evaluate(
    policy: &SafetyPolicy,
    command: &str,
    mode: super::adapters::LaunchMode,
) -> Outcome {
    evaluate_with_scratchpad_roots(policy, command, mode, &[], None, None, 0)
}

/// Evaluate with scratchpad and worker-envelope scope so both constraints
/// apply to VCS cleanup and policy decisions. Keep the envelope explicit
/// rather than reading process environment inside pure evaluation (#262).
pub(crate) fn evaluate_with_scratchpad_roots(
    policy: &SafetyPolicy,
    command: &str,
    mode: super::adapters::LaunchMode,
    scratchpad_roots: &[String],
    envelope: Option<&envelope::WorkerEnvelope>,
    cwd: Option<&Path>,
    now: u64,
) -> Outcome {
    let canonical = canonical_shell_syntax(command);
    let command = canonical.as_deref().unwrap_or(command);
    let mut base = evaluate_candidates(
        policy,
        command,
        policy.default_verdict(mode),
        mode,
        scratchpad_roots,
    );
    // Syntax the scanners cannot model may hide a live command: never Allow it.
    if canonical.is_none() && base.verdict == Verdict::Allow {
        base = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "<shell: ambiguous quoting or comment>".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
    }
    apply_envelope_outcome(envelope, command, scratchpad_roots, cwd, now, base)
}

/// A worker cannot exceed its delegated envelope even when ordinary policy
/// allows the command; force Deny outside that scope (#262).
fn apply_envelope_outcome(
    envelope: Option<&envelope::WorkerEnvelope>,
    command: &str,
    scratchpad_roots: &[String],
    cwd: Option<&Path>,
    now: u64,
    base: Outcome,
) -> Outcome {
    let Some(envelope) = envelope else {
        return base;
    };
    let deny = |reason: &'static str| Outcome {
        verdict: Verdict::Deny,
        matched: Some(Rule {
            pattern: format!("<envelope: {reason}>"),
            origin: Origin::BuiltIn,
        }),
    };
    if !envelope.tools.shell {
        return deny("shell tool not granted");
    }
    if envelope.expires_at < now {
        return deny("expired");
    }
    let candidates = normalize_segments(command);
    if (!envelope.network || !envelope.tools.network)
        && candidates.iter().any(|candidate| {
            sql_tokens(candidate)
                .and_then(|tokens| {
                    tokens
                        .first()
                        .map(|first| is_network_program(&sql_program_name(first)))
                })
                .unwrap_or(false)
        })
    {
        return deny("network not granted");
    }
    if !envelope.destructive
        && candidates
            .iter()
            .any(|candidate| command_is_destructive(candidate, scratchpad_roots))
    {
        return deny("destructive command outside the delegation envelope");
    }
    if envelope_write_targets_confined(command, envelope, cwd) == Some(false) {
        return deny("write target outside the delegation envelope's paths");
    }
    base
}

/// Reuse existing destructive classifiers so envelope restrictions track
/// the same operations as ordinary safety policy (#262).
pub(super) fn command_is_destructive(command: &str, scratchpad_roots: &[String]) -> bool {
    is_recursive_delete(command)
        || is_destructive_orchestrator_action(command)
        || is_irreversible_distribution_action(command)
        || is_destructive_vcs_action(command, scratchpad_roots)
}

/// Check definite write targets against the worker path scope. Unknown
/// targets and commands without targets produce no inferred violation (#262).
fn envelope_write_targets_confined(
    command: &str,
    envelope: &envelope::WorkerEnvelope,
    cwd: Option<&Path>,
) -> Option<bool> {
    let sanitized = redact_single_quoted_heredocs(command);
    let resolve = |path: &str| {
        let path = match cwd {
            Some(cwd) => resolve_repo_write_target(path, &cwd.to_string_lossy())?,
            None => {
                let normalized = path.replace('\\', "/");
                let absolute = Path::new(&normalized).is_absolute();
                let resolved = super::pathutil::canonicalize_with_missing_tail(
                    &Path::new(".").join(&normalized),
                )?;
                return Some((
                    envelope::PathScope::new(resolved.to_string_lossy()),
                    absolute,
                ));
            }
        };
        let path = path.replace('\\', "/");
        let absolute = path.starts_with('/')
            || (path.as_bytes().get(1) == Some(&b':') && path.as_bytes().get(2) == Some(&b'/'));
        Some((envelope::PathScope::new(path), absolute))
    };
    let roots = envelope
        .paths
        .iter()
        .filter_map(|root| resolve(&root.0))
        .collect::<Vec<_>>();
    let mut confined = true;
    let mut saw_any_target = false;
    for segment in normalize_segments(&sanitized) {
        let targets = segment_write_targets(&segment)?;
        for target in &targets {
            saw_any_target = true;
            if target == "/dev/null" {
                continue;
            }
            if target.contains(['$', '`', '~', '*', '?']) {
                return None;
            }
            let (scope, absolute) = resolve(target)?;
            if !roots
                .iter()
                .any(|(root, root_absolute)| absolute == *root_absolute && scope.is_subset_of(root))
            {
                confined = false;
            }
        }
    }
    if !saw_any_target {
        return None;
    }
    Some(confined)
}

/// Parse the worker envelope from the environment. Absence means no worker
/// scope; a malformed present value uses the locked envelope, never an
/// unrestricted fallback (#262).
pub(crate) fn parse_envelope_env(env: EnvLookup<'_>) -> Option<envelope::WorkerEnvelope> {
    let raw = env(super::agent::ENVELOPE_ENV)?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(serde_json::from_str(&raw).unwrap_or_else(|_| envelope::WorkerEnvelope::locked()))
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    /// Task 1 (issue #168): `evaluate_candidate_outcome` must reproduce
    /// exactly what `evaluate_candidates`'s own fold already does for a
    /// single, unambiguous candidate -- this is a refactor extracting the
    /// analyzer chain, not a behavior change.
    #[test]
    fn evaluate_candidate_outcome_matches_the_existing_fold_for_a_single_candidate() {
        let policy = SafetyPolicy::default();
        for (command, expected) in [
            ("git status", Verdict::Allow),
            ("git push --force origin main", Verdict::Ask),
            // Under the shipped default policy, plain `rm -rf` is an ASK
            // family (`Bash(rm -rf *)` in `SHIPPED_POSTURE_ASK`) -- only
            // `rm -rf*zirv*` is DENY. This row locks in that actual default
            // behavior through the extracted chain, not a hypothetical one.
            ("rm -rf /", Verdict::Ask),
            ("rm -rf /home/user/zirv", Verdict::Deny),
            ("some-totally-unknown-tool --flag", Verdict::Ask),
        ] {
            let direct = evaluate_candidate_outcome(&policy, command, command, Verdict::Ask, &[]);
            let via_evaluate_candidates =
                evaluate_candidates(&policy, command, Verdict::Ask, LaunchMode::Headless, &[]);
            assert_eq!(direct.verdict, expected, "{command}");
            assert_eq!(
                direct.verdict, via_evaluate_candidates.verdict,
                "{command}: extracted chain must agree with the fold"
            );
        }
    }

    #[test]
    fn zirv_path_delete_deny_only_inspects_the_recursive_delete_segment() {
        let policy = SafetyPolicy::default();
        let unrelated = evaluate(
            &policy,
            "rm -rf /tmp/unrelated; zirv frontend check --help",
            LaunchMode::Interactive,
        );
        assert_ne!(
            unrelated.verdict,
            Verdict::Deny,
            "a later Zirv command is not an rm target: {unrelated:?}"
        );

        let direct = evaluate(&policy, "rm -rf ~/.zirv", LaunchMode::Interactive);
        assert_eq!(direct.verdict, Verdict::Deny, "got {direct:?}");
        assert_eq!(
            direct.matched.as_ref().map(|rule| rule.pattern.as_str()),
            Some("rm -rf*zirv*")
        );
    }

    // -- recursive delete of a temp-scratch target (headless `dontAsk`
    // denial fix, 72-run sample) ------------------------------------------

    /// A bare `rm -rf` of an absolute path under `/tmp` is the common
    /// headless-agent-cleaning-up-after-itself shape and must be allowed in
    /// every launch mode, not just interactive -- the whole point is that a
    /// headless session (`--permission-mode dontAsk`) cannot answer an `Ask`
    /// at all.
    #[test]
    fn recursive_delete_of_an_absolute_temp_target_is_allowed() {
        let policy = SafetyPolicy::default();
        for mode in [LaunchMode::Interactive, LaunchMode::Headless] {
            let outcome = evaluate(&policy, "rm -rf /tmp/ledgerlite_doc_test", mode);
            assert_eq!(outcome.verdict, Verdict::Allow, "{mode:?}: {outcome:?}");
        }
    }

    /// Bug fix: `apply_recursive_delete_outcome`'s `overridable` check used
    /// to accept ANY `Origin::BuiltIn` `Ask` -- including `apply_credential_
    /// outcome`'s own built-in `"<project secret file read>"` rule -- not
    /// only the shipped `"rm -rf *"`/`"rm -fr *"` glob it was meant to widen.
    /// A recursive delete whose target is BOTH confined to temp AND names a
    /// project secret (a real headless shape: an agent cleaning up its own
    /// scratch clone, which happens to carry a `.env`) was silently widened
    /// from `Ask` to `Allow`, defeating the credential guard entirely. This
    /// must keep asking; an ordinary temp-scratch delete with no secret in
    /// its target must still be allowed exactly as before.
    #[test]
    fn recursive_delete_of_a_project_secret_under_temp_still_asks() {
        let policy = SafetyPolicy::default();
        let secret = evaluate(&policy, "rm -rf /tmp/x/.env", LaunchMode::Interactive);
        assert_eq!(
            secret.verdict,
            Verdict::Ask,
            "a recursive delete targeting a project secret must keep asking: {secret:?}"
        );

        let plain = evaluate(&policy, "rm -rf /tmp/x", LaunchMode::Interactive);
        assert_eq!(
            plain.verdict,
            Verdict::Allow,
            "an ordinary temp-scratch recursive delete must still be allowed: {plain:?}"
        );
    }

    /// The exact shape from the 72-run sample: a relative delete target
    /// resolved against the compound's own leading `cd /tmp` -- the case
    /// [`recursive_delete_confined_to_temp`] resolves via
    /// [`parse_leading_cd_segment`] applied to the WHOLE original command,
    /// since `normalize_segments` has already split `cd /tmp` away from `rm
    /// -rf lltest` by the time either reaches `apply_recursive_delete_outcome`.
    ///
    /// Interactive mode, deliberately: a bare `cd /tmp` segment has no
    /// shipped allow/ask rule of its own, so under the shipped policy's
    /// `default: Ask` for HEADLESS it would independently ask regardless of
    /// this fix, which is a fact about `cd`'s own lack of a rule, not
    /// something this fix is scoped to change. `interactive_default: Allow`
    /// keeps that unrelated fact out of this test, isolating exactly the
    /// recursive-delete widening this fix adds; the real headless hook path
    /// exercises the mode-sensitivity separately via `evaluate_candidates`'
    /// own `mode` parameter, unaffected by this change either way.
    #[test]
    fn recursive_delete_of_a_relative_target_under_a_leading_cd_tmp_is_allowed() {
        let policy = SafetyPolicy::default();
        let outcome = evaluate(&policy, "cd /tmp && rm -rf lltest", LaunchMode::Interactive);
        assert_eq!(outcome.verdict, Verdict::Allow, "got {outcome:?}");
    }

    /// Another exact shape from the 72-run sample, this time with the delete
    /// as the FIRST segment and no leading `cd` at all: `rm -rf /tmp/ll2 &&
    /// mkdir -p /tmp/ll2 && ...`. `normalize_segments` always carries the
    /// WHOLE raw command as its own first candidate (see its own doc
    /// comment), so this compound is itself one of the candidates
    /// `evaluate_candidates` folds -- and since it literally starts with
    /// `rm -rf `, it independently matches the built-in `rm -rf *` ask glob
    /// too, on top of the properly split-out `rm -rf /tmp/ll2` segment.
    /// Before `delete_targets` stopped at the chain separator, that
    /// whole-command candidate's "targets" swallowed the chained tokens
    /// (`"&&"`, `"mkdir"`, `"echo"`, `"ok"`, ...) too, so its own confinement
    /// check always failed and its `Ask` outvoted the correctly-`Allow`ed
    /// split candidate in the worst-of-all-candidates fold -- reproduced
    /// live via a real headless `zirv ctx exec` probe, not just this
    /// classifier's own return value.
    #[test]
    fn recursive_delete_of_a_temp_target_followed_by_chained_commands_is_allowed() {
        let policy = SafetyPolicy::default();
        for mode in [LaunchMode::Interactive, LaunchMode::Headless] {
            let outcome = evaluate(
                &policy,
                "rm -rf /tmp/ll2 && mkdir -p /tmp/ll2 && echo done",
                mode,
            );
            assert_eq!(outcome.verdict, Verdict::Allow, "{mode:?}: {outcome:?}");
        }
    }

    /// `..` must not escape the temp root: `/tmp/../etc` lexically resolves
    /// to `/etc`, which is not confined to any temp root, so this stays
    /// exactly as restrictive as an ordinary `rm -rf /etc` -- `Ask` under the
    /// shipped default posture.
    #[test]
    fn recursive_delete_cannot_escape_the_temp_root_via_dot_dot() {
        let policy = SafetyPolicy::default();
        let outcome = evaluate(&policy, "rm -rf /tmp/../etc", LaunchMode::Interactive);
        assert_eq!(outcome.verdict, Verdict::Ask, "got {outcome:?}");
    }

    /// Deleting the temp root itself is not a throwaway-scratch cleanup --
    /// `path_strictly_below` requires the target to be STRICTLY below a temp
    /// root, never equal to one.
    #[test]
    fn recursive_delete_of_the_temp_root_itself_stays_ask() {
        let policy = SafetyPolicy::default();
        let outcome = evaluate(&policy, "rm -rf /tmp", LaunchMode::Interactive);
        assert_eq!(outcome.verdict, Verdict::Ask, "got {outcome:?}");
    }

    /// A target with no leading `cd` and no absolute prefix cannot be
    /// resolved against anything -- this must stay exactly as restrictive as
    /// today rather than assume the process's own cwd is confined.
    #[test]
    fn recursive_delete_of_an_unresolvable_relative_target_stays_ask() {
        let policy = SafetyPolicy::default();
        let outcome = evaluate(&policy, "rm -rf src", LaunchMode::Interactive);
        assert_eq!(outcome.verdict, Verdict::Ask, "got {outcome:?}");
    }

    /// A shell variable in the target cannot be trusted from the text alone
    /// -- `$X` could expand to anything, including a path outside every temp
    /// root -- so this verdict is UNCHANGED by the fix: still `Ask`, exactly
    /// as it was before this fix existed.
    #[test]
    fn recursive_delete_of_a_target_carrying_a_shell_variable_is_unchanged() {
        let policy = SafetyPolicy::default();
        let outcome = evaluate(&policy, "rm -rf /tmp/$X", LaunchMode::Interactive);
        assert_eq!(outcome.verdict, Verdict::Ask, "got {outcome:?}");
    }

    /// The existing `rm -rf*zirv*` DENY still wins even for a target that
    /// also happens to be confined to a temp root: [`apply_recursive_delete_
    /// outcome`] only ever reconsiders a `base.verdict == Allow`, and a
    /// target naming `zirv` already produced `Deny` upstream of it.
    #[test]
    fn recursive_delete_deny_for_a_zirv_named_target_wins_even_under_tmp() {
        let policy = SafetyPolicy::default();
        let outcome = evaluate(&policy, "rm -rf /tmp/zirv-stuff", LaunchMode::Interactive);
        assert_eq!(outcome.verdict, Verdict::Deny, "got {outcome:?}");
    }

    // -- issue #168, decision (f): find/locate/rg never hard-deny --------

    /// No policy layer -- built-in, and none of this task's new classifiers
    /// -- ever produces `Deny` for an ordinary read-only `find`/`locate`/
    /// `rg` invocation, across both launch modes, both permission modes
    /// hook-mode cares about, and both values of `dangerouslyDisableSandbox`.
    /// `find -exec`/`-ok` running something unproven still legitimately
    /// escalates to `Ask` (`is_risky_find_exec`) -- this only pins the
    /// FLOOR, never `Deny`, for the read-only shapes below.
    #[test]
    fn read_only_find_locate_rg_never_hard_deny_via_plain_evaluate() {
        let policy = SafetyPolicy::default();
        for command in [
            "find . -name '*.rs'",
            "find ./src -iname '*.md' -type f",
            "find / -name id_rsa",
            "find ~ -name '*.pem'",
            "locate id_rsa",
            "locate -i '*.env'",
            "rg TODO .",
            "rg --hidden -i password .",
            "find . -exec grep -l TODO {} +",
            "find . -exec sed -n '1p' {} +",
        ] {
            for mode in [LaunchMode::Interactive, LaunchMode::Headless] {
                let outcome = evaluate(&policy, command, mode);
                assert_ne!(
                    outcome.verdict,
                    Verdict::Deny,
                    "{command} ({mode:?}) must never hard-deny: {outcome:?}"
                );
            }
        }
    }

    /// `dangerouslyDisableSandbox` is deliberately held at `false` here (no
    /// unsandboxed retry involved) -- decision (f) is about the BASE
    /// classifier never hard-denying these read-only shapes (locked in via
    /// plain `evaluate()` above; this test additionally routes the same
    /// claim through the full hook pipeline: attestation, the `cd`-prefix
    /// strip, and the scratchpad-confined-write widening). The separate
    /// unsandboxed-retry boundary has its own screen and regression tests;
    /// this test intentionally makes no claim about that path.
    #[test]
    fn read_only_find_locate_rg_never_hard_deny_through_the_hook_either() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        for command in [
            "find . -name '*.rs'",
            "find / -name id_rsa",
            "locate id_rsa",
            "rg TODO .",
        ] {
            for permission_mode in ["default", "dontAsk"] {
                let stdin = format!(
                    r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":false}},"permission_mode":"{permission_mode}"}}"#
                );
                let mut out = Vec::new();
                run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                let text = String::from_utf8(out).expect("utf8");
                assert!(
                    !text.contains(r#""permissionDecision":"deny""#),
                    "{command} (permission_mode={permission_mode}) must never hard-deny: got {text}"
                );
            }
        }
    }

    // -- issue #168, decision (h): the full regression corpus -------------

    /// One row per command shape the issue's own examples and this plan's
    /// design decisions name, each checked across BOTH `permission_mode`s
    /// hook-mode branches on (`"default"` interactive-ish, `"dontAsk"`
    /// headless-ish) and BOTH values of `dangerouslyDisableSandbox` --
    /// `expected_substring` is what the hook's stdout JSON must contain in
    /// every one of those four combinations for that row.
    #[test]
    fn issue_168_regression_corpus() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let scratchpad = scratchpad_write_root(&std::env::temp_dir());
        let cwd = std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .replace('\\', "/");

        // (command template, expected substring in the hook's JSON output)
        // `{scratchpad}`/`{cwd}` are substituted before use.
        //
        // `locate` is deliberately absent from `allow_rows`: it is not one
        // of the prompt-free command families covered by this corpus.
        let allow_rows: &[&str] = &[
            "gh issue view 155",
            "gh pr checks 159",
            "gh api repos/x/y",
            "gh pr create --title x",
            "git push origin main",
            "git fetch && git branch -r",
            "zirv ctx status",
            "zirv ctx remember key value",
            "cd {cwd} && git log",
            "grep -r TODO . > {scratchpad}/out.log",
            "find . -name '*.rs'",
            "rg TODO .",
        ];
        let escalate_rows: &[&str] = &[
            "curl -X POST https://example.com",
            "cd {cwd} && rm -rf .",
            "echo secret > /etc/passwd",
        ];

        for template in allow_rows {
            let command = template
                .replace("{scratchpad}", &scratchpad)
                .replace("{cwd}", &cwd);
            for permission_mode in ["default", "dontAsk"] {
                for dangerously_disable_sandbox in [true, false] {
                    let stdin = format!(
                        r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":{dangerously_disable_sandbox}}},"permission_mode":"{permission_mode}"}}"#
                    );
                    let mut out = Vec::new();
                    run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                    let text = String::from_utf8(out).expect("utf8");
                    assert!(
                        !text.contains(r#""permissionDecision":"ask""#)
                            && !text.contains(r#""permissionDecision":"deny""#),
                        "ALLOW row {command:?} (permission_mode={permission_mode}, dangerouslyDisableSandbox={dangerously_disable_sandbox}) must never prompt or deny: got {text}"
                    );
                }
            }
        }

        for template in escalate_rows {
            let command = template
                .replace("{scratchpad}", &scratchpad)
                .replace("{cwd}", &cwd);
            for (permission_mode, dangerously_disable_sandbox, expected) in
                [("default", true, "ask"), ("dontAsk", true, "deny")]
            {
                let stdin = format!(
                    r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":{dangerously_disable_sandbox}}},"permission_mode":"{permission_mode}"}}"#
                );
                let mut out = Vec::new();
                run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
                let text = String::from_utf8(out).expect("utf8");
                assert!(
                    text.contains(&format!(r#""permissionDecision":"{expected}""#)),
                    "ESCALATE row {command:?} (permission_mode={permission_mode}, dangerouslyDisableSandbox={dangerously_disable_sandbox}) expected {expected}: got {text}"
                );
            }
        }

        // The outer `kubectl exec` family is allowed, but an unsandboxed retry
        // whose decoded inner command is a bare interactive shell stays
        // ambiguous: ask with a human present, deny headlessly.
        for (permission_mode, expected) in [("default", "ask"), ("dontAsk", "deny")] {
            let stdin = format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":"kubectl exec -it pod -- sh","dangerouslyDisableSandbox":true}},"permission_mode":"{permission_mode}"}}"#
            );
            let mut out = Vec::new();
            run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                text.contains(&format!(r#""permissionDecision":"{expected}""#)),
                "permission_mode={permission_mode} expected {expected}: got {text}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_directory_cleanup_asks_when_a_literal_target_or_ancestor_is_a_symlink() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let cfg = CtxConfig::load(repo.path(), &|_| None).unwrap();
        std::fs::create_dir(repo.path().join("real")).unwrap();
        std::fs::create_dir(repo.path().join("target")).unwrap();
        std::os::unix::fs::symlink(repo.path().join("real"), repo.path().join("node_modules"))
            .unwrap();
        for (command, decision) in [
            ("rm -rf node_modules", "ask"),
            ("rm -rf node_modules/subdir", "ask"),
            ("rm -rf target", "allow"),
        ] {
            let stdin = serde_json::json!({"tool_name":"Bash", "tool_input":{"command":command},
                "cwd":repo.path(), "permission_mode":"default"})
            .to_string();
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|_| None).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(
                value["hookSpecificOutput"]["permissionDecision"], decision,
                "{command}"
            );
            if decision == "ask" {
                assert!(
                    value["hookSpecificOutput"]["permissionDecisionReason"]
                        .as_str()
                        .unwrap()
                        .contains("generated-directory symlink")
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn envelope_write_scopes_resolve_symlinks_before_comparing_containment() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        std::fs::create_dir(cwd.join("allowed")).unwrap();
        std::fs::create_dir(cwd.join("outside")).unwrap();
        std::fs::write(cwd.join("allowed/plain"), "").unwrap();
        std::os::unix::fs::symlink(cwd.join("outside"), cwd.join("allowed/link")).unwrap();
        std::os::unix::fs::symlink(cwd.join("allowed"), cwd.join("scope-link")).unwrap();
        std::os::unix::fs::symlink(cwd.join("missing"), cwd.join("allowed/dangling")).unwrap();
        let mut envelope = safety_test_envelope();
        for (scope, target, expected) in [
            ("allowed", "allowed/link/passwd", Some(false)),
            ("allowed", "allowed/link/../escaped", Some(false)),
            ("allowed", "allowed/plain", Some(true)),
            ("allowed", "allowed/new-file", Some(true)),
            ("allowed", "allowed/new-dir/new-file", Some(true)),
            ("scope-link", "allowed/new-file", Some(true)),
            ("scope-link", "allowed/link/passwd", Some(false)),
            ("allowed", "allowed/dangling/file", None),
        ] {
            envelope.paths = vec![envelope::PathScope::new(scope)];
            assert_eq!(
                envelope_write_targets_confined(
                    &format!("echo x > {target}"),
                    &envelope,
                    Some(cwd)
                ),
                expected,
                "{scope}: {target}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn an_unresolvable_envelope_root_does_not_disable_write_confinement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path();
        std::fs::create_dir(cwd.join("allowed")).expect("allowed root");
        std::os::unix::fs::symlink(cwd.join("missing"), cwd.join("dangling"))
            .expect("dangling root");
        let mut envelope = safety_test_envelope();
        envelope.paths.push(envelope::PathScope::new("dangling"));
        for (command, confined) in [
            ("echo x > allowed/new-file", true),
            ("echo x > outside", false),
        ] {
            assert_eq!(
                envelope_write_targets_confined(command, &envelope, Some(cwd)),
                Some(confined),
                "{command}"
            );
        }
        envelope.paths = vec![envelope::PathScope::new("dangling")];
        assert_eq!(
            envelope_write_targets_confined("echo x > outside", &envelope, Some(cwd)),
            Some(false)
        );
    }

    #[test]
    fn envelope_write_scopes_resolve_relative_and_absolute_targets_against_the_hook_cwd() {
        let mut envelope = safety_test_envelope();
        for (scope, target, cwd, confined) in [
            ("allowed", "allowed/out.txt", Some("/w"), true),
            ("allowed", "/w/allowed/out.txt", Some("/w"), true),
            ("allowed", "/allowed/out.txt", Some("/w"), false),
            ("allowed", "../out.txt", Some("/w"), false),
            ("allowed", "allowed/../allowed/out.txt", Some("/w"), true),
            (".", "/w/anything", Some("/w"), true),
            (".", "sub/file", Some("/w"), true),
            (".", "/etc/passwd", Some("/w"), false),
            ("allowed", "/allowed/out.txt", None, false),
            ("/allowed", "allowed/out.txt", None, false),
            ("allowed", "allowed/out.txt", None, true),
            ("/allowed", "/allowed/out.txt", None, true),
        ] {
            envelope.paths = vec![envelope::PathScope::new(scope)];
            assert_eq!(
                envelope_write_targets_confined(
                    &format!("echo x > {target}"),
                    &envelope,
                    cwd.map(Path::new)
                ),
                Some(confined),
                "{scope}: {target}"
            );
        }
    }

    #[test]
    fn file_mutation_destinations_cannot_escape_the_envelopes_paths() {
        let envelope = safety_test_envelope();
        for command in [
            "cp input.txt outside.txt",
            "mv input.txt outside.txt",
            "ln input.txt outside.txt",
            "install -m 600 input.txt outside.txt",
            "rsync -a input.txt outside.txt",
            "touch outside.txt allowed/file",
            "mkdir -p outside allowed/dir",
            "truncate -s 0 outside.txt allowed/file",
            "rm outside.txt allowed/file",
            "sed -i 's/a/b/' outside.txt",
            "sed -i -e 's/a/b/' outside.txt allowed/file",
            "dd if=input.txt of=outside.txt",
            "command cp input.txt outside.txt",
            "sh -c 'touch outside.txt'",
            "cp -t outside input.txt",
        ] {
            assert_eq!(
                envelope_write_targets_confined(command, &envelope, Some(Path::new("/w"))),
                Some(false),
                "{command}"
            );
        }
        for command in [
            "cp a allowed/b",
            "touch allowed/b",
            "mkdir -m 700 allowed/dir",
            "truncate -s 0 allowed/b",
            "sed -i -e 's/a/b/' allowed/b",
            "dd of=allowed/b",
            "cp -t allowed a",
            "sed 's/a/b/' outside.txt",
        ] {
            assert_ne!(
                envelope_write_targets_confined(command, &envelope, Some(Path::new("/w"))),
                Some(false),
                "{command}"
            );
        }
    }

    #[test]
    fn envelope_shell_network_and_expiry_grants_are_independent_hard_denials() {
        let policy = SafetyPolicy::default();
        let check = |envelope: &envelope::WorkerEnvelope, command| {
            evaluate_with_scratchpad_roots(
                &policy,
                command,
                LaunchMode::Interactive,
                &[],
                Some(envelope),
                Some(Path::new("/w")),
                2,
            )
        };
        let mut envelope = safety_test_envelope();
        envelope.tools.shell = false;
        let outcome = check(&envelope, "echo allowed");
        assert_eq!(outcome.verdict, Verdict::Deny);
        assert_eq!(
            outcome.matched.unwrap().pattern,
            "<envelope: shell tool not granted>"
        );
        envelope.tools.shell = true;
        assert_eq!(check(&envelope, "echo allowed").verdict, Verdict::Allow);
        for (network, tool) in [(false, true), (true, false)] {
            envelope.network = network;
            envelope.tools.network = tool;
            for command in [
                "curl https://example.invalid",
                "command wget https://example.invalid",
                "echo x; curl http://localhost",
                "sh -c 'curl https://example.invalid'",
            ] {
                let outcome = check(&envelope, command);
                assert_eq!(outcome.verdict, Verdict::Deny, "{command}");
                assert_eq!(
                    outcome.matched.unwrap().pattern,
                    "<envelope: network not granted>"
                );
            }
            assert_eq!(check(&envelope, "echo allowed").verdict, Verdict::Allow);
        }
        envelope.expires_at = 1;
        let outcome = check(&envelope, "echo allowed");
        assert_eq!(outcome.verdict, Verdict::Deny);
        assert_eq!(outcome.matched.unwrap().pattern, "<envelope: expired>");
        for expires_at in [2, 3, u64::MAX] {
            envelope.expires_at = expires_at;
            assert_eq!(check(&envelope, "echo allowed").verdict, Verdict::Allow);
        }
        for command in [
            "git push --force",
            "xargs git push --force",
            "echo x; git push --force",
        ] {
            assert_eq!(
                check(&envelope, command).verdict,
                Verdict::Deny,
                "{command}"
            );
        }
    }
}
