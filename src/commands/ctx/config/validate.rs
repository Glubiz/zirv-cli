use super::*;

/// Require an anchor on every top-level regex alternative; `^a|b` can match unrelated commands (#417).
/// Escapes, character classes and nested groups do not split alternatives; harmless leading flags are skipped.
pub(crate) fn is_fully_anchored(pattern: &str) -> bool {
    split_top_level_alternatives(pattern)
        .into_iter()
        .all(starts_with_anchor)
}

/// Split only outside groups and character classes, respecting escapes.
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

/// Accept `^` or `\A` after harmless inline flags; `\A` anchors absolutely even under multiline mode.
fn starts_with_anchor(alternative: &str) -> bool {
    let mut rest = alternative;
    while let Some(after) = strip_one_inline_flag_group(rest) {
        rest = after;
    }
    rest.starts_with('^') || rest.starts_with("\\A")
}

/// Never strip a flag group enabling `m`: `^` could then match embedded command lines such as heredocs.
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

/// Flags after `-` are disabled, so merely containing `m` does not enable multiline matching.
fn flag_group_enables_multiline(flags: &str) -> bool {
    let disable_at = flags.find('-');
    let enabled_part = match disable_at {
        Some(idx) => &flags[..idx],
        None => flags,
    };
    enabled_part.contains('m')
}

/// Scan every inline and scoped flag group: multiline mode anywhere can bypass command-start anchoring.
fn contains_multiline_enabling_flag_group(pattern: &str) -> bool {
    let mut idx = 0usize;
    while let Some(rel) = pattern[idx..].find("(?") {
        let start = idx + rel + 2;
        let after = &pattern[start..];
        // Only `(?flags)` and `(?flags:...)` set flags; do not mistake other group constructs for them.
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
        // Always advance to avoid finding the same non-match forever.
        idx = start + flag_len.max(1);
        if idx > pattern.len() {
            break;
        }
    }
    false
}

/// Validate regexes, command anchors and unique rule names after config merge; apply-time code trusts these bounds (#417).
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
        // Reject multiline mode before the generic anchor check to explain the bypass precisely:
        // `^` could match a heredoc line inside an unrelated command.
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

/// Shared argv-bound model guard blocks flag injection and Windows cmd.exe reparsing attacks.
/// Dashboard model requests must use the same charset, length and leading-dash checks.
pub(crate) fn validate_model_str(key: &str, model: &str) -> CtxResult<()> {
    if model.is_empty()
        || model.len() > 128
        // Reject a leading dash so model values cannot become flags; mid-id hyphens remain valid.
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

/// Validate endpoint settings with named errors; never read or print the auth variable's value (#395).
/// Only its shell-identifier name is checked here; adapter readiness resolves it at launch.
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
            if !vendor.rungs.is_empty()
                && super::super::catalogue::rung_of_known(vendor, model).is_none()
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

/// Share URL metacharacter validation so harness and native endpoint defenses cannot drift.
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
