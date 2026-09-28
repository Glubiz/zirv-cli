//! Policy rules for command safety.

use super::*;

/// A minimal glob matcher: `*` matches any run of characters (including
/// none), every other character matches itself literally, case-sensitively
/// (shell commands are case-sensitive). No `?`, no character classes -- the
/// small vocabulary issue #83's own examples use (`"rm -rf /*"`, `"git push
/// --force*"`, `"* | sh"`).
///
/// Iterative two-pointer matching with a saved star position (the standard
/// `fnmatch`-style algorithm), not recursive backtracking: a command string
/// can originate from repository-influenced text (a prompt-injected shell
/// command an agent was talked into proposing), so this must not be a
/// stack-depth or exponential-blowup DoS surface. Worst case is `O(pattern
/// * command)` with no recursion.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Issue #106: claude's own documented prefix semantics for a
    // `<verb> *`-style rule match the bare verb too (`Bash(git *)` "matches
    // git, git status, git commit" -- adapters/mod.rs's own doc comment),
    // but the star here otherwise only matches text *after* the literal
    // space that precedes it, so `"git push --force *"` matched `"git push
    // --force x"` yet not the bare `"git push --force"` a real invocation
    // sends with nothing following. Every `verb *` deny pattern was
    // therefore inert against exactly that bare form. A pattern ending in
    // `" *"` also matches its own prefix with the trailing `" *"` stripped.
    if let Some(prefix) = pattern.strip_suffix(" *")
        && text == prefix
    {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_from = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            match_from = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(star_pi) = star {
            pi = star_pi + 1;
            match_from += 1;
            ti = match_from;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The raw `[safety]` table shape as written in one `ctx.toml` layer.
/// Deliberately distinct from [`SafetyPolicy`] (the *effective*, built-in
/// -inclusive policy): this is only ever what one layer's file text says.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SafetyLayer {
    deny: Vec<String>,
    ask: Vec<String>,
    allow: Vec<String>,
    escape_allow: Vec<String>,
    default: Option<Verdict>,
    interactive_default: Option<Verdict>,
    sql: Option<SqlMode>,
    denial_breaker_threshold: Option<u32>,
    identical_command_warn_after: Option<u32>,
    identical_command_refuse_after: Option<u32>,
}

fn parse_layer(layer: Option<toml::Value>, origin: &str) -> CtxResult<SafetyLayer> {
    let Some(layer) = layer else {
        return Ok(SafetyLayer::default());
    };
    layer.try_into().map_err(|e| {
        let error_msg = e.to_string().replace('\n', " ");
        format!("{origin}: invalid [safety] section: {error_msg}").into()
    })
}

fn rules_from(patterns: &[String], origin: Origin) -> Vec<Rule> {
    patterns
        .iter()
        .cloned()
        .map(|pattern| Rule { pattern, origin })
        .collect()
}

/// Issue #313: the repo-narrowing fold for the three loop-breaker
/// thresholds (`denial_breaker_threshold`/`identical_command_warn_after`/
/// `identical_command_refuse_after`), mirroring `config.rs`'s own
/// `narrow_max_nudges` (`home.min(repo.unwrap_or(u32::MAX))`) but with `0`
/// carrying its own meaning ("disabled") rather than "unbounded", so plain
/// `min` cannot be used unmodified:
///
/// - `home == 0`: the operator disabled this breaker outright. A repo may
///   only narrow, never re-enable something the operator turned off, so this
///   stays `0` regardless of what `repo` says (unlike `narrow_max_nudges`,
///   where `home == 0` is just an ordinary, narrowable value).
/// - `repo == Some(0)`: a repo trying to set `0` is trying to WIDEN (disable
///   the breaker), which is never narrowing -- ignored, exactly like a
///   `None`.
/// - Otherwise: `home.min(repo)` -- a repo may lower the threshold (fire the
///   breaker sooner) but never raise it above the operator's own ceiling.
fn narrow_threshold(home: u32, repo: Option<u32>) -> u32 {
    if home == 0 {
        return 0;
    }
    match repo {
        Some(0) | None => home,
        Some(repo) => home.min(repo),
    }
}

/// Resolves the layered `[safety]` policy -- see the module doc for the
/// fold. `home`/`repo` are the `[safety]` tables lifted out of `~/.zirv/
/// ctx.toml` and `<repo>/.zirv/ctx.toml` by `CtxConfig::load` (either
/// absent when that file has no `[safety]` section) before its own deep
/// merge; `env` is the operator override that sits above both.
///
/// `repo`'s own `allow`/`escape_allow`/`default` fields are never read here,
/// even if present: `config::reject_untrusted_keys` already hard-errors a
/// repo file that sets any of them before this function is ever reached
/// (see `REPO_FORBIDDEN`), so by the time a `repo` value arrives here it is
/// guaranteed not to carry them -- this is defense in depth, not the
/// primary enforcement.
pub fn resolve(
    home: Option<toml::Value>,
    repo: Option<toml::Value>,
    env: EnvLookup<'_>,
) -> CtxResult<SafetyPolicy> {
    let home_layer = parse_layer(home, "~/.zirv/ctx.toml")?;
    let repo_layer = parse_layer(repo, "<repo>/.zirv/ctx.toml")?;

    let deny = match env("ZIRV_CTX_SAFETY_DENY") {
        Some(raw) => {
            let mut deny = builtin_deny();
            deny.extend(rules_from(&split_csv_list(&raw), Origin::Env));
            deny
        }
        None => {
            let mut deny = builtin_deny();
            deny.extend(rules_from(&home_layer.deny, Origin::Operator));
            deny.extend(rules_from(&repo_layer.deny, Origin::Repo));
            deny
        }
    };

    let ask = match env("ZIRV_CTX_SAFETY_ASK") {
        Some(raw) => {
            let mut ask = builtin_ask();
            ask.extend(rules_from(&split_csv_list(&raw), Origin::Env));
            ask
        }
        None => {
            let mut ask = builtin_ask();
            ask.extend(rules_from(&home_layer.ask, Origin::Operator));
            ask.extend(rules_from(&repo_layer.ask, Origin::Repo));
            ask
        }
    };

    let allow = match env("ZIRV_CTX_SAFETY_ALLOW") {
        Some(raw) => {
            let mut allow = builtin_allow();
            allow.extend(rules_from(&split_csv_list(&raw), Origin::Env));
            allow
        }
        None => {
            let mut allow = builtin_allow();
            allow.extend(rules_from(&home_layer.allow, Origin::Operator));
            allow
        }
    };

    // Issue #147: `escape_allow` gets the identical operator-home-layer-only
    // treatment as `allow` above (see `REPO_FORBIDDEN`'s `safety.escape_allow`
    // entry and this arm never reads `repo_layer.escape_allow` -- the same
    // defense in depth `allow` already has), plus a built-in seed
    // (`builtin_escape_allow`) `allow` has none of.
    let escape_allow = match env("ZIRV_CTX_SAFETY_ESCAPE_ALLOW") {
        Some(raw) => {
            let mut escape_allow = builtin_escape_allow();
            escape_allow.extend(rules_from(&split_csv_list(&raw), Origin::Env));
            escape_allow
        }
        None => {
            let mut escape_allow = builtin_escape_allow();
            escape_allow.extend(rules_from(&home_layer.escape_allow, Origin::Operator));
            escape_allow
        }
    };

    let default = match env("ZIRV_CTX_SAFETY_DEFAULT") {
        Some(raw) => Verdict::parse(&raw).ok_or_else(|| {
            format!("ZIRV_CTX_SAFETY_DEFAULT: expected allow, ask or deny, got '{raw}'")
        })?,
        None => home_layer.default.unwrap_or(Verdict::Ask),
    };

    let interactive_default = match env("ZIRV_CTX_SAFETY_INTERACTIVE_DEFAULT") {
        Some(raw) => Verdict::parse(&raw).ok_or_else(|| {
            format!("ZIRV_CTX_SAFETY_INTERACTIVE_DEFAULT: expected allow, ask or deny, got '{raw}'")
        })?,
        // Home-layer only, exactly like `default` above: this key is
        // `REPO_FORBIDDEN`, so a repo value can never reach this function --
        // and this arm never reads `repo_layer.interactive_default`, the
        // same defense in depth `allow`/`default` already have.
        None => home_layer.interactive_default.unwrap_or(Verdict::Allow),
    };

    let sql = match env("ZIRV_CTX_SAFETY_SQL") {
        Some(raw) => SqlMode::parse(&raw)
            .ok_or_else(|| format!("ZIRV_CTX_SAFETY_SQL: expected on or off, got '{raw}'"))?,
        // Home-layer only, exactly like `default`/`interactive_default`
        // above: this key is `REPO_FORBIDDEN`, and this arm never reads
        // `repo_layer.sql` -- the same defense in depth `allow` already has.
        None => home_layer.sql.unwrap_or_default(),
    };

    // Issue #313: the two loop breakers' three thresholds get the identical
    // narrowing fold `config.rs`'s own `narrow_max_nudges` uses for
    // `verify_on_stop.max_nudges` -- lower is stricter -- via
    // `narrow_threshold` (this module's own `0`-means-disabled variant of
    // that fold; see its doc comment). No environment override today: unlike
    // `deny`/`ask`/`allow`, nothing yet needs an operator escape hatch above
    // the fold for these, and one can be added later without disturbing this
    // shape.
    let denial_breaker_threshold = narrow_threshold(
        home_layer.denial_breaker_threshold.unwrap_or(3),
        repo_layer.denial_breaker_threshold,
    );
    let identical_command_warn_after = narrow_threshold(
        home_layer.identical_command_warn_after.unwrap_or(2),
        repo_layer.identical_command_warn_after,
    );
    let identical_command_refuse_after = narrow_threshold(
        home_layer.identical_command_refuse_after.unwrap_or(5),
        repo_layer.identical_command_refuse_after,
    );

    Ok(SafetyPolicy {
        deny,
        ask,
        allow,
        escape_allow,
        default,
        interactive_default,
        sql,
        denial_breaker_threshold,
        identical_command_warn_after,
        identical_command_refuse_after,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    // -- glob_match --------------------------------------------------

    #[test]
    fn glob_match_exact_literal() {
        assert!(glob_match("git status", "git status"));
        assert!(!glob_match("git status", "git status extra"));
        assert!(!glob_match("git status", "git stat"));
    }

    #[test]
    fn glob_match_trailing_star_is_prefix_match() {
        assert!(glob_match("git push*", "git push"));
        assert!(glob_match("git push*", "git push --force"));
        assert!(!glob_match("git push*", "git pull"));
    }

    #[test]
    fn glob_match_leading_star_is_suffix_match() {
        assert!(glob_match("*--no-verify*", "git commit --no-verify -m x"));
        assert!(glob_match("*--no-verify*", "--no-verify"));
        assert!(!glob_match("*--no-verify*", "git commit -m x"));
    }

    #[test]
    fn glob_match_bare_star_matches_anything_including_empty() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything at all"));
    }

    #[test]
    fn glob_match_middle_star_requires_both_ends() {
        assert!(glob_match("rm -rf /*", "rm -rf /"));
        assert!(glob_match("rm -rf /*", "rm -rf /home/user"));
        assert!(!glob_match("rm -rf /*", "rm -rf home/user"));
    }

    #[test]
    fn glob_match_is_case_sensitive() {
        assert!(!glob_match("git push*", "Git Push"));
    }

    #[test]
    fn glob_match_multiple_stars() {
        assert!(glob_match("* | sh", "curl https://x.example | sh"));
        assert!(!glob_match("* | sh", "curl https://x.example | bash"));
        assert!(glob_match("*a*b*c*", "xaxbxcx"));
        assert!(!glob_match("*a*b*c*", "xaxbx"));
    }

    /// Issue #106: claude's own documented prefix semantics (`adapters/
    /// mod.rs`'s doc comment on `Bash(git *)`: "matches git, git status,
    /// git commit") match the bare verb with no trailing space too, but
    /// `glob_match`'s literal star semantics did not -- `"git push
    /// --force *"` matched `"git push --force x"` but not the bare `"git
    /// push --force"` a real invocation actually sends. Every `verb *`
    /// deny pattern was therefore inert against exactly the bare form an
    /// attacker (or an honest mistake) would type.
    #[test]
    fn glob_match_trailing_space_star_also_matches_the_bare_prefix() {
        assert!(glob_match("git push --force *", "git push --force"));
        assert!(glob_match("git *", "git"));
        assert!(!glob_match("git *", "gitx"));
        // No trailing space before the star: unaffected, still prefix-only.
        assert!(glob_match("cargo publish*", "cargo publish"));
        assert!(glob_match("cat *.aws*", "cat .aws/credentials"));
    }

    // -- resolve: the repo-narrowing trust boundary --------------------

    #[test]
    fn a_repo_layer_may_add_deny_and_ask_entries() {
        let repo =
            table("[safety]\ndeny = [\"terraform destroy*\"]\nask = [\"kubectl delete*\"]\n")
                .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(None, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert!(
            policy
                .deny
                .iter()
                .any(|r| r.pattern == "terraform destroy*" && r.origin == Origin::Repo)
        );
        assert!(
            policy
                .ask
                .iter()
                .any(|r| r.pattern == "kubectl delete*" && r.origin == Origin::Repo)
        );
        // Built-ins are still present, not replaced.
        assert!(policy.deny.iter().any(|r| r.origin == Origin::BuiltIn));
    }

    /// SECURITY: a repo `[safety]` table cannot set `allow` or `default` at
    /// all -- `config::CtxConfig::load` rejects the whole layer outright
    /// before `resolve` is ever reached (see `REPO_FORBIDDEN` in
    /// `config.rs`), so this module's own `resolve` never receives a `repo`
    /// value carrying either field in production. This test pins the
    /// defense-in-depth half: even if a caller handed `resolve` a `repo`
    /// value that somehow carried `allow`/`default` (a malformed caller, not
    /// a real code path), this function must still never read them.
    #[test]
    fn resolve_never_reads_allow_or_default_from_the_repo_layer() {
        let repo = table(
            "[safety]\nallow = [\"rm -rf /*\"]\ndefault = \"allow\"\ndeny = [\"echo narrow\"]\n",
        )
        .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(None, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert!(
            !policy
                .allow
                .iter()
                .any(|r| r.pattern == "rm -rf /*" && r.origin == Origin::Repo),
            "a repo-carried allow entry must never be read"
        );
        assert_eq!(
            policy.default,
            Verdict::Ask,
            "a repo-carried default must never be read"
        );
        assert!(policy.deny.iter().any(|r| r.pattern == "echo narrow"));
    }

    /// Issue #147, defense in depth for `escape_allow`: identical to
    /// `resolve_never_reads_allow_or_default_from_the_repo_layer` above.
    #[test]
    fn resolve_never_reads_escape_allow_from_the_repo_layer() {
        let repo =
            table("[safety]\nescape_allow = [\"curl *\"]\n").and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(None, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert!(
            !policy
                .escape_allow
                .iter()
                .any(|r| r.pattern == "curl *" && r.origin == Origin::Repo),
            "a repo-carried escape_allow entry must never be read"
        );
    }

    #[test]
    fn the_operator_may_add_allow_entries_and_change_the_default() {
        let home = table("[safety]\nallow = [\"just test*\"]\ndefault = \"deny\"\n")
            .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(home, None, &|k| empty.get(k).cloned()).expect("resolves");
        assert!(
            policy
                .allow
                .iter()
                .any(|r| r.pattern == "just test*" && r.origin == Origin::Operator)
        );
        assert_eq!(policy.default, Verdict::Deny);
    }

    /// Issue #147: `escape_allow` resolves the identical way `allow` does
    /// above -- home layer only, additive to the built-in seed -- and the
    /// built-in read-only-utility seed is always present regardless.
    #[test]
    fn the_operator_may_add_escape_allow_entries_on_top_of_the_builtin_seed() {
        let home = table("[safety]\nescape_allow = [\"just test*\"]\n")
            .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(home, None, &|k| empty.get(k).cloned()).expect("resolves");
        assert!(
            policy
                .escape_allow
                .iter()
                .any(|r| r.pattern == "just test*" && r.origin == Origin::Operator)
        );
        assert!(
            policy
                .escape_allow
                .iter()
                .any(|r| r.pattern == "grep *" && r.origin == Origin::BuiltIn),
            "the built-in read-only-utility seed must still be present: {:?}",
            policy.escape_allow
        );
    }

    #[test]
    fn the_environment_may_replace_the_contributed_escape_allow_list_but_keeps_builtins() {
        let home = table("[safety]\nescape_allow = [\"just home*\"]\n")
            .and_then(|v| v.get("safety").cloned());
        let vars = env_from(&[("ZIRV_CTX_SAFETY_ESCAPE_ALLOW", "just env-only*")]);
        let policy = resolve(home, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert!(
            !policy
                .escape_allow
                .iter()
                .any(|r| r.pattern == "just home*")
        );
        assert!(
            policy
                .escape_allow
                .iter()
                .any(|r| r.pattern == "just env-only*" && r.origin == Origin::Env)
        );
        assert!(
            policy
                .escape_allow
                .iter()
                .any(|r| r.pattern == "grep *" && r.origin == Origin::BuiltIn),
            "the built-in seed survives the env override too: {:?}",
            policy.escape_allow
        );
    }

    #[test]
    fn the_environment_replaces_the_contributed_deny_list_but_keeps_builtins() {
        let home =
            table("[safety]\ndeny = [\"echo home\"]\n").and_then(|v| v.get("safety").cloned());
        let vars = env_from(&[("ZIRV_CTX_SAFETY_DENY", "echo env-only")]);
        let policy = resolve(home, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert!(!policy.deny.iter().any(|r| r.pattern == "echo home"));
        assert!(
            policy
                .deny
                .iter()
                .any(|r| r.pattern == "echo env-only" && r.origin == Origin::Env)
        );
        assert!(
            policy.deny.iter().any(|r| r.origin == Origin::BuiltIn),
            "env override must not remove built-in protections"
        );
    }

    // -- issue #313: loop-breaker threshold narrowing fold ---------------

    /// The pure fold, pinned directly: lower always wins when both layers
    /// set a nonzero value, a repo `0` (an attempt to WIDEN by disabling) is
    /// ignored, and an operator `0` (disabled) can never be turned back on
    /// by a repo.
    #[test]
    fn narrow_threshold_lets_a_repo_lower_but_never_raise_or_reenable() {
        assert_eq!(narrow_threshold(3, Some(2)), 2, "repo may narrow");
        assert_eq!(narrow_threshold(3, Some(5)), 3, "repo may not raise");
        assert_eq!(
            narrow_threshold(3, Some(0)),
            3,
            "a repo 0 is an attempted widening (disable) -- ignored"
        );
        assert_eq!(narrow_threshold(3, None), 3, "no repo value: home stands");
        assert_eq!(
            narrow_threshold(0, Some(2)),
            0,
            "operator-disabled stays disabled -- a repo cannot re-enable it"
        );
        assert_eq!(narrow_threshold(0, None), 0);
    }

    #[test]
    fn a_repo_layer_may_narrow_but_not_raise_the_denial_breaker_threshold() {
        let home = table("[safety]\ndenial_breaker_threshold = 3\n")
            .and_then(|v| v.get("safety").cloned());
        let repo = table("[safety]\ndenial_breaker_threshold = 2\n")
            .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(home, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert_eq!(policy.denial_breaker_threshold, 2, "repo may narrow to 2");

        let repo_wider = table("[safety]\ndenial_breaker_threshold = 5\n")
            .and_then(|v| v.get("safety").cloned());
        let home_again = table("[safety]\ndenial_breaker_threshold = 3\n")
            .and_then(|v| v.get("safety").cloned());
        let policy_wider =
            resolve(home_again, repo_wider, &|k| empty.get(k).cloned()).expect("resolves");
        assert_eq!(
            policy_wider.denial_breaker_threshold, 3,
            "repo may not raise the threshold above home's own"
        );
    }

    #[test]
    fn a_repo_zero_denial_breaker_threshold_is_ignored_as_an_attempted_widening() {
        let home = table("[safety]\ndenial_breaker_threshold = 3\n")
            .and_then(|v| v.get("safety").cloned());
        let repo = table("[safety]\ndenial_breaker_threshold = 0\n")
            .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(home, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert_eq!(
            policy.denial_breaker_threshold, 3,
            "a repo 0 would disable the breaker -- that is widening, not narrowing"
        );
    }

    #[test]
    fn an_operator_disabled_denial_breaker_cannot_be_reenabled_by_a_repo() {
        let home = table("[safety]\ndenial_breaker_threshold = 0\n")
            .and_then(|v| v.get("safety").cloned());
        let repo = table("[safety]\ndenial_breaker_threshold = 2\n")
            .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(home, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert_eq!(
            policy.denial_breaker_threshold, 0,
            "an operator who disabled the breaker cannot have it re-enabled by a repo checkout"
        );
    }

    #[test]
    fn the_two_identical_command_guard_thresholds_follow_the_identical_narrowing_fold() {
        let home = table(
            "[safety]\nidentical_command_warn_after = 2\nidentical_command_refuse_after = 5\n",
        )
        .and_then(|v| v.get("safety").cloned());
        let repo = table(
            "[safety]\nidentical_command_warn_after = 1\nidentical_command_refuse_after = 9\n",
        )
        .and_then(|v| v.get("safety").cloned());
        let empty = env_from(&[]);
        let policy = resolve(home, repo, &|k| empty.get(k).cloned()).expect("resolves");
        assert_eq!(
            policy.identical_command_warn_after, 1,
            "repo may narrow warn_after"
        );
        assert_eq!(
            policy.identical_command_refuse_after, 5,
            "repo may not raise refuse_after above home's own"
        );
    }

    #[test]
    fn the_default_loop_breaker_thresholds_match_the_documented_defaults() {
        let policy = SafetyPolicy::default();
        assert_eq!(policy.denial_breaker_threshold, 3);
        assert_eq!(policy.identical_command_warn_after, 2);
        assert_eq!(policy.identical_command_refuse_after, 5);
    }

    #[test]
    fn an_unparseable_default_env_value_is_an_error_not_a_silent_default() {
        let vars = env_from(&[("ZIRV_CTX_SAFETY_DEFAULT", "sometimes")]);
        let err = resolve(None, None, &|k| vars.get(k).cloned()).expect_err("must reject");
        assert!(err.to_string().contains("ZIRV_CTX_SAFETY_DEFAULT"));
    }
}
