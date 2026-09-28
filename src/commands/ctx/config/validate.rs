use super::*;

/// Whether every top-level `|` alternative in `pattern` starts with `^`.
/// Issue #417: `[[output.filter]]`'s `match_command` is required to be fully
/// anchored -- `gradle` would also match `my-not-gradle-thing`, which is
/// almost certainly not what an operator naming a program meant -- and
/// "starts with `^`" has to be checked per top-level alternative, not on the
/// pattern as a whole, because `^a|b` is unanchored on its `b` branch even
/// though the string itself starts with `^`.
///
/// Splits on `|` at nesting depth 0 relative to `(...)` groups and outside
/// any `[...]` character class, honouring `\`-escapes so an escaped `\|` or
/// `\[` never it self toggles class/group state. A leading `(?flags)` inline
/// modifier group (e.g. `(?i)^a`) is stripped before the `^` check, since it
/// is common and does not weaken the anchor. `(^a|b)` -- one top-level
/// alternative, the whole parenthesized group, which does not itself start
/// with `^` -- is correctly rejected; `^a|^b`, `(?i)^a` and `^(a|b)` are all
/// accepted.
pub(crate) fn is_fully_anchored(pattern: &str) -> bool {
    split_top_level_alternatives(pattern)
        .into_iter()
        .all(starts_with_anchor)
}

/// Splits `pattern` on every top-level `|` (see [`is_fully_anchored`]'s own
/// doc comment for exactly what "top-level" means here).
fn split_top_level_alternatives(pattern: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_class = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (idx, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => depth -= 1,
            '|' if !in_class && depth <= 0 => {
                parts.push(&pattern[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&pattern[start..]);
    parts
}

/// Whether `alternative` starts with `^` or `\A`, after skipping past zero or
/// more leading `(?flags)` inline modifier groups (letters/`-` only between
/// `(?` and `)`, e.g. `(?i)`, `(?is)`, `(?-i)`) -- those do not weaken an
/// anchor, so `(?i)^a` counts as anchored the same as plain `^a`. `\A` (the
/// regex crate's "absolute start of haystack" anchor) is accepted alongside
/// `^` since it anchors even under the `m` flag, where `^` would not.
fn starts_with_anchor(alternative: &str) -> bool {
    let mut rest = alternative;
    while let Some(after) = strip_one_inline_flag_group(rest) {
        rest = after;
    }
    rest.starts_with('^') || rest.starts_with("\\A")
}

/// Review finding: a leading inline flag group is only harmless to strip
/// past when it does not itself enable the multiline flag `m` -- `(?m)^a` is
/// NOT fully anchored, because under `m`, `^` matches at the start of every
/// line, not just the start of the whole haystack (and `hook::run_posttool`
/// composes a command line that can itself contain embedded newlines, e.g. a
/// heredoc Bash command). So this only strips a flag group whose `m` is
/// either absent or explicitly disabled (`(?-m)`); a group that enables `m`
/// (`(?m)`, `(?im)`, `(?i-m)` does NOT count as enabling it since `-m` wins)
/// is left in place, which makes `starts_with_anchor` correctly see a `(`,
/// not a `^`, and reject the pattern as unanchored.
fn strip_one_inline_flag_group(s: &str) -> Option<&str> {
    let body = s.strip_prefix("(?")?;
    let end = body.find(')')?;
    let flags = &body[..end];
    if !flags.is_empty()
        && flags.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
        && !flag_group_enables_multiline(flags)
    {
        Some(&body[end + 1..])
    } else {
        None
    }
}

/// Whether an inline flag group's flag list (the text between `(?` and `)`,
/// e.g. `"im"`, `"i-m"`, `"-m"`) enables the multiline flag `m` -- i.e. `m`
/// appears before any `-`, or there is no `-` at all and `m` appears. Once a
/// `-` is seen, every flag after it is being DISABLED, so `(?-m)` and
/// `(?i-m)` do not enable `m` even though the letter appears in the string.
fn flag_group_enables_multiline(flags: &str) -> bool {
    let disable_at = flags.find('-');
    let enabled_part = match disable_at {
        Some(idx) => &flags[..idx],
        None => flags,
    };
    enabled_part.contains('m')
}

/// Review finding: `is_fully_anchored`'s per-alternative check above is
/// defeated if ANY inline flag group in the whole pattern enables `m`,
/// wherever it appears -- not just a leading one `strip_one_inline_flag_
/// group` walks past. A group later in the pattern (`^a|(?m)^b`) still makes
/// every subsequent `^` in the SAME regex match at any line start once the
/// regex crate applies it, so `validate_output_filter_rules` scans the whole
/// `match_command` string for one, rather than relying solely on the leading-
/// group walk. Matches `(?flags)` and `(?flags:...)` (a scoped group), since
/// both syntaxes enable flags for what follows.
fn contains_multiline_enabling_flag_group(pattern: &str) -> bool {
    let mut idx = 0usize;
    while let Some(rel) = pattern[idx..].find("(?") {
        let start = idx + rel + 2;
        let after = &pattern[start..];
        // The flags body is the run of ASCII letters/`-` right after `(?`;
        // it is a real inline flag group only when that run is immediately
        // followed by `)` (`(?flags)`) or `:` (`(?flags:...)`, a scoped
        // group) -- anything else (`(?:...)`, `(?=...)`, `(?<name>...)`,
        // ...) is a different construct entirely and must not be misread as
        // one.
        let flag_len = after
            .bytes()
            .take_while(|&b| b.is_ascii_alphabetic() || b == b'-')
            .count();
        let flags = &after[..flag_len];
        let terminator = after.as_bytes().get(flag_len).copied();
        if !flags.is_empty()
            && matches!(terminator, Some(b')') | Some(b':'))
            && flag_group_enables_multiline(flags)
        {
            return true;
        }
        // Advance past this `(?` occurrence (by at least one byte) so a
        // non-match can't loop forever re-finding the same spot.
        idx = start + flag_len.max(1);
        if idx > pattern.len() {
            break;
        }
    }
    false
}

/// Load-time validation for `[[output.filter]]` (issue #417): every regex
/// must compile, `match_command` must be fully anchored
/// ([`is_fully_anchored`]), and no two rules may share a `name` -- every
/// error names the offending rule so an operator can find it without
/// guessing which of several is at fault. Called once from `CtxConfig::load`
/// after the layers are merged; never re-checked at apply time in
/// `output.rs`, which trusts a config that reached this point.
pub(crate) fn validate_output_filter_rules(rules: &[OutputFilterRule]) -> CtxResult<()> {
    let mut seen_names = std::collections::HashSet::new();
    for rule in rules {
        if !seen_names.insert(rule.name.as_str()) {
            return Err(format!(
                "output.filter \"{}\": duplicate rule name -- every [[output.filter]] entry needs \
                 a unique `name`",
                rule.name
            )
            .into());
        }
        if let Err(e) = regex::Regex::new(&rule.match_command) {
            return Err(format!(
                "output.filter \"{}\": match_command {:?} is not a valid regex: {e}",
                rule.name, rule.match_command
            )
            .into());
        }
        // Review finding: checked before the generic anchor check below so
        // an operator sees the specific, actionable reason -- a leading
        // `^` under the `m` flag anchors at any LINE start, not the start
        // of the whole command line, and `hook::run_posttool`'s composed
        // command line can itself contain embedded newlines (a heredoc Bash
        // command), so an `m`-enabled match_command can match a line deep
        // inside an unrelated command.
        if contains_multiline_enabling_flag_group(&rule.match_command) {
            return Err(format!(
                "output.filter \"{}\": match_command must not enable the multiline flag (m): \
                 ^ must anchor the whole command line",
                rule.name
            )
            .into());
        }
        if !is_fully_anchored(&rule.match_command) {
            return Err(format!(
                "output.filter \"{}\": match_command must be fully anchored (every top-level \
                 alternative starts with `^`), got {:?}",
                rule.name, rule.match_command
            )
            .into());
        }
        for pattern in &rule.strip_lines {
            if let Err(e) = regex::Regex::new(pattern) {
                return Err(format!(
                    "output.filter \"{}\": strip_lines pattern {pattern:?} is not a valid regex: \
                     {e}",
                    rule.name
                )
                .into());
            }
        }
        for pattern in &rule.keep_lines {
            if let Err(e) = regex::Regex::new(pattern) {
                return Err(format!(
                    "output.filter \"{}\": keep_lines pattern {pattern:?} is not a valid regex: \
                     {e}",
                    rule.name
                )
                .into());
            }
        }
        if let Some(match_output) = &rule.match_output
            && let Err(e) = regex::Regex::new(&match_output.pattern)
        {
            return Err(format!(
                "output.filter \"{}\": match_output.pattern {:?} is not a valid regex: {e}",
                rule.name, match_output.pattern
            )
            .into());
        }
    }
    Ok(())
}

/// SECURITY (command-injection defense): shared charset/length/leading-dash
/// guard for every argv-bound model string this config exposes (`chat.model`,
/// `review.claude`, `review.codex`, `worker.claude`, `worker.codex`) -- see
/// the call site above `chat.model`'s own doc comment for the full Windows
/// cmd.exe-reparse threat model this defends against. `key` is the dotted
/// config path named in the returned error, so a caller can tell which of
/// several model fields failed.
///
/// `pub(crate)`: `dash/mod.rs`'s `pane_model_args` also needs this exact
/// guard, for the same reason -- a dashboard spawn request's `model` reaches
/// a launch argv just like `worker.claude`/`worker.codex` do, so it gets the
/// same charset/length/leading-dash check rather than a second, possibly
/// drifting copy of it.
pub(crate) fn validate_model_str(key: &str, model: &str) -> CtxResult<()> {
    if model.is_empty()
        || model.len() > 128
        // A leading `-` would let the value pose as its own flag on the
        // launch argv (`--model --dangerously-skip-permissions`), so it is
        // rejected even though `-` is otherwise a legal model-id character.
        // Anchored here rather than dropped from the charset, since a hyphen
        // mid-id (`claude-opus-5`) is legitimate.
        || model.starts_with('-')
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/' | '@'))
    {
        return Err(format!(
            "invalid ctx config: `{key}` may contain only ASCII letters, digits and `-._:/@` and \
             may not begin with `-`, got '{model}'"
        )
        .into());
    }
    Ok(())
}

/// Issue #395: load-time validation for one `[endpoint.claude]`/`[endpoint.
/// codex]` table. Named errors -- `key` prefixes every message with the
/// dotted table path (`"endpoint.claude"`), so an operator with both tables
/// misconfigured sees which one failed. Never reads or prints
/// `credential_env`'s VALUE -- only its own name is validated, and only as a
/// shell-identifier shape (`AgentAdapter::ready()` is what checks the named
/// variable actually resolves to a non-empty secret, at launch time, not
/// here).
pub(super) fn validate_endpoint_target(key: &str, target: &EndpointTarget) -> CtxResult<()> {
    let vendor = super::super::catalogue::vendor(&target.vendor).ok_or_else(|| {
        let known: Vec<&str> = super::super::catalogue::vendors()
            .iter()
            .map(|v| v.slug)
            .collect();
        format!(
            "{key}: vendor \"{}\" is not a catalogue vendor (known: {})",
            target.vendor,
            known.join(", ")
        )
    })?;

    validate_endpoint_base_url(key, &target.base_url)?;

    if target.credential_env.is_empty()
        || target.credential_env.contains('=')
        || !target
            .credential_env
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "{key}: credential_env must be a non-empty environment variable NAME (ASCII \
             letters, digits, underscore, never `=` or the secret itself), got \"{}\"",
            target.credential_env
        )
        .into());
    }

    if let Some(wire_api) = target.wire_api.as_deref()
        && wire_api != "chat"
        && wire_api != "responses"
    {
        return Err(format!(
            "{key}: wire_api must be \"chat\" or \"responses\", got \"{wire_api}\""
        )
        .into());
    }

    match target.model.as_deref() {
        Some(model) => {
            if !vendor.rungs.is_empty() && super::super::catalogue::rung_of(vendor, model).is_none()
            {
                return Err(format!(
                    "{key}: model \"{model}\" does not resolve (by alias or id) on vendor \
                     \"{}\"'s catalogue ladder",
                    target.vendor
                )
                .into());
            }
        }
        None => {
            if vendor.rungs.is_empty() {
                return Err(format!(
                    "{key}: model is required for vendor \"{}\", which has no catalogue rungs \
                     to default from",
                    target.vendor
                )
                .into());
            }
        }
    }

    Ok(())
}

/// Shared base-URL validation for harness overrides and native provider
/// endpoints. The metacharacter rule lives in one place so the two config
/// surfaces cannot drift.
pub(crate) fn validate_endpoint_base_url(key: &str, base_url: &str) -> CtxResult<()> {
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(format!("{key}: base_url must be an http(s) URL, got \"{base_url}\"").into());
    }
    if let Some(bad) = base_url.chars().find(|c| {
        c.is_whitespace()
            || *c == '\''
            || *c == '"'
            || super::super::adapters::CMD_REPARSE_METACHARS.contains(c)
    }) {
        return Err(format!(
            "{key}: base_url must not contain {bad:?} (it is passed to codex as a -c argv \
             token)"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit coverage for `is_fully_anchored` itself, independent of
    /// `CtxConfig::load` -- see its own doc comment for exactly what "every
    /// top-level alternative starts with `^`" means.
    #[test]
    fn is_fully_anchored_checks_every_top_level_alternative() {
        assert!(is_fully_anchored("^a|^b"));
        assert!(is_fully_anchored("(?i)^a"));
        assert!(is_fully_anchored("^(a|b)"));
        assert!(is_fully_anchored("^(\\./)?gradlew?\\b"));
        assert!(!is_fully_anchored("(^a|b)"));
        assert!(!is_fully_anchored("gradle"));
        assert!(!is_fully_anchored("^a|b"));
    }

    /// Review finding: a `(?flags)` inline modifier group that enables the
    /// multiline flag `m` defeats the whole point of requiring `^` to
    /// anchor `match_command` -- under `m`, `^` matches at the start of
    /// every LINE, not just the start of the whole command line, and
    /// `hook::run_posttool`'s composed command line can itself contain
    /// embedded newlines (a heredoc Bash command). Refused by name,
    /// wherever the group appears (`^a|(?m)^b` is refused even though its
    /// FIRST alternative is anchored); `(?i)^gradle`, `(?-m)^gradle` (`m`
    /// explicitly disabled) and `\Agradle` (an absolute-start anchor immune
    /// to `m` in the first place) are all still accepted.
    #[test]
    fn multiline_flag_group_defeats_the_anchor_check_and_is_refused() {
        for bad in ["(?m)^gradle", "(?im)^gradle", "^a|(?m)^b"] {
            let rule = OutputFilterRule {
                name: "bad-rule".to_string(),
                match_command: bad.to_string(),
                strip_lines: Vec::new(),
                keep_lines: Vec::new(),
                truncate_line_at: None,
                max_lines: None,
                match_output: None,
            };
            let err = validate_output_filter_rules(&[rule])
                .expect_err(&format!("{bad:?} must be refused"));
            assert!(
                err.to_string().contains("bad-rule"),
                "the error must name the rule for {bad:?}: {err}"
            );
            assert!(
                err.to_string().contains("multiline"),
                "the error must explain why for {bad:?}: {err}"
            );
        }

        for good in ["(?i)^gradle", "(?-m)^gradle", "\\Agradle"] {
            let rule = OutputFilterRule {
                name: "ok-rule".to_string(),
                match_command: good.to_string(),
                strip_lines: Vec::new(),
                keep_lines: Vec::new(),
                truncate_line_at: None,
                max_lines: None,
                match_output: None,
            };
            assert!(
                validate_output_filter_rules(&[rule]).is_ok(),
                "{good:?} must be accepted"
            );
        }
    }
}
