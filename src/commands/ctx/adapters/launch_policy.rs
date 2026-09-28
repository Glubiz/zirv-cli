//! Sandbox/permission policy: the shipped allow/deny/ask posture, scratchpad
//! rules, and the launch argv built from an `EffectivePolicy`.
use super::*;

/// Whether `flags` already pins one of the CLI-level policy flags
/// `AgentAdapter::policy_args`/`default_sandbox_args` might otherwise
/// prepend: claude's `--disallowedTools`/`--allowedTools`/`--permission-
/// mode`, or codex's `-s/--sandbox` and `-a/--ask-for-approval`. Exact match
/// or the `=`-joined form only (`--sandbox=read-only`) -- unlike `classify_
/// model_flag`, this deliberately does not also recognise an attached short
/// form (`-sread-only`): that spelling was never verified for these flags
/// the way `-mvalue` was for `--model` (`-m`'s own attached form is a
/// dedicated, tested case in this file), and a false positive here means
/// silently *withholding* zirv's own restriction rather than merely
/// mis-ordering a model flag, so precision matters more than coverage.
///
/// **Codex `-c`/`--config` overrides (2026-08-26, approval-posture round):**
/// an operator may also pin codex's approval/sandbox posture via a raw
/// config override -- `-c approval_policy=<value>`, `-c sandbox_mode=<value>`,
/// or the long `--config` spelling of either -- rather than the dedicated
/// `--ask-for-approval`/`--sandbox` flags above. Before this, that spelling
/// was invisible to this function: `flags_pin_policy` returned `false`, so
/// `policy_launch_args` still prepended zirv's own `-c approval_policy=...`
/// (`CodexAdapter::approval_suppression_args`'s exec-probe fallback) *after*
/// the operator's own override, and codex's config resolution is
/// last-value-wins, so the operator's explicit choice was silently
/// overridden by zirv's own. `CODEX_CONFIG_OVERRIDE_KEYS` below closes that
/// gap for the split form (`-c`/`--config` followed by a `key=value` token,
/// checked pairwise since the key lives in the *next* token, unlike every
/// other flag this function recognises), the `=`-joined single-token form
/// (`--config=approval_policy=...`), mirroring the existing `--sandbox=...`
/// handling above, and (2026-08-26, correction round) codex's own attached
/// short form -- `-cKEY=VALUE` with no space at all, verified accepted by
/// codex-cli 0.149.1, mirroring the attached `-mvalue` form
/// `classify_model_flag` already recognises for `--model`. All-or-nothing,
/// same as every other flag this function recognises: pinning just one
/// dimension (e.g. only `approval_policy`) withholds zirv's *entire*
/// computed prefix, not only the approval half -- `policy_launch_args` has
/// no partial-prefix concept, and this function's contract has always been
/// "the operator's own flag pins policy outright".
///
/// Mirrors `agent.rs`'s own `flags_pin_model` in spirit: the operator's own
/// explicit choice must demonstrably win over a zirv-computed default, not
/// merely happen to survive because a CLI takes the last occurrence of a
/// repeated flag. `policy_launch_args` is the sole caller that acts on this.
///
/// **`--dangerously-skip-permissions`/`--dangerously-bypass-approvals-and-
/// sandbox` (issue #224 review round 2):** claude's and codex's own bare
/// "remove every check" toggles pin the loosest possible posture outright,
/// the same way a dedicated `--permission-mode`/`--sandbox` value does --
/// leaving them out meant `policy_launch_args` still prepended zirv's own
/// (functionally inert, since these toggles win regardless of position)
/// prefix ahead of them, and `safety::reserved_zirv_auto_allow_rule` had no
/// way to see that a `zirv agent`/`zirv chat` invocation's forwarded flags
/// had asked the spawned harness to drop its own guardrails.
pub fn flags_pin_policy(flags: &[String]) -> bool {
    const POLICY_FLAG_NAMES: &[&str] = &[
        "--disallowedTools",
        "--allowedTools",
        "--permission-mode",
        "--settings",
        "--sandbox",
        "-s",
        "--ask-for-approval",
        "-a",
        "--approve-for-me",
        "--dangerously-skip-permissions",
        "--dangerously-bypass-approvals-and-sandbox",
    ];
    const CODEX_CONFIG_OVERRIDE_KEYS: &[&str] =
        &["approval_policy", "sandbox_mode", "default_permissions"];

    let names_a_config_override_key = |value: &str| {
        CODEX_CONFIG_OVERRIDE_KEYS
            .iter()
            .any(|key| value == *key || value.starts_with(&format!("{key}=")))
    };

    flags.iter().enumerate().any(|(i, f)| {
        if POLICY_FLAG_NAMES
            .iter()
            .any(|name| f == name || f.starts_with(&format!("{name}=")))
        {
            return true;
        }
        if let Some(rest) = f.strip_prefix("--config=") {
            return names_a_config_override_key(rest);
        }
        if f == "-c" || f == "--config" {
            return flags
                .get(i + 1)
                .is_some_and(|next| names_a_config_override_key(next));
        }
        // Codex's attached short form (`-cKEY=VALUE`, no space) -- checked
        // after the `f == "-c"` split form above so a bare `-c` still falls
        // through to that pairwise check rather than being consumed here
        // with an empty attached value.
        if let Some(rest) = f.strip_prefix("-c")
            && !rest.is_empty()
            && names_a_config_override_key(rest)
        {
            return true;
        }
        false
    })
}

/// One family of in-repo-development or destructive actions zirv's own
/// shipped-default "sandboxed, no prompts" posture takes a position on --
/// the shared static source both `ClaudeAdapter::default_sandbox_args` (which
/// projects every entry onto a concrete `Bash(...)`/`Read(...)`/`Edit(...)`
/// permission rule) and codex's own `default_sandbox_args` (a coarse
/// `--sandbox workspace-write --ask-for-approval never` pair, documented
/// against this same list -- see that method's own doc comment) are
/// expressions of, so the two harnesses' postures cannot independently
/// drift into disagreement about what "sandboxed, no prompts" means. Zirv's
/// case-insensitive reserved built-ins are the one generated family alongside
/// this constant; `safety::reserved_zirv_command_patterns` derives them from
/// the dispatch layer's `utils::RESERVED_COMMANDS` source of truth.
///
/// **Why `dontAsk` alone is not enough (2026-08-22, fix round 2):** a fresh
/// install with no operator-configured `permissions.allow` denies every
/// `Write`/`Edit`/`Bash` call outright -- safe, but inert, not the "session
/// works and stays safe" posture the operator actually asked for. This list
/// is what makes `--permission-mode dontAsk` *usable* out of the box.
///
/// **Verified live, not guessed**, against the installed `claude 2.1.240`:
/// - `Edit(./**)` (not bare `Write`) is the rule that actually scopes a
///   write to the workspace -- the CLI's own runtime error is explicit
///   about this: `"Write(./**) is not matched by file permission checks --
///   only Edit(path) rules are. ... Edit rules cover all file-editing
///   tools."` A bare `Write` allow rule, tested live, let a write reach the
///   *parent* directory of the workspace with no denial at all.
/// - `Read(./**)` genuinely scopes reads to the workspace (a read outside
///   it was denied); a bare `Read` rule, tested live, did not (it read a
///   file one directory above the workspace).
/// - A `disallowedTools` entry wins over a broader, unrelated `allowedTools`
///   entry even when both could apply to the same command family (`Bash(git
///   push --force *)` denied while a broader `Bash(git *)` allow was also
///   configured) -- Claude Code's own settings.json schema documents `deny`
///   winning over `allow` as the contract, and this was reproduced live, not
///   assumed from the docs alone.
/// - `Bash(<verb> *)` is prefix matching (Claude Code's own embedded schema
///   docs: `"Prefix wildcard: \"Bash(git *)\" - matches git, git status, git
///   commit, etc."`), reproduced live for both the space-separated form
///   (`Bash(git status *)`, the documented spelling) and a colon-separated
///   form that also happened to work; this list uses the documented
///   spelling.
///
/// **What is deliberately NOT in this list, and why:** "writes outside the
/// workspace" is not a separate deny rule -- `Edit(./**)`'s own scoping
/// already denies it by omission (verified live above), and a second rule
/// trying to express the same negative space would be redundant and harder
/// to audit. General credential-file reads via `Bash(cat ...)` are denied
/// the same way: no allow rule pre-approves `cat`/`Bash` in general, so
/// `dontAsk` denies it by omission; `Bash(security *)` is still listed
/// explicitly (the one credential-reading *command family* worth naming on
/// its own, since zirv's own macOS keychain fallback already documents it
/// as the concrete vector -- see `poll.rs`).
///
/// **Fix round 4 (2026-08-23, issue #104): whole toolchain families, harness
/// dirs, scratchpad, `WebFetch`/`WebSearch`.** Round 2's list above still hit
/// `dontAsk`'s own inert-by-omission failure one layer up: it only
/// pre-approved a handful of subcommands per toolchain (`cargo build *`/
/// `cargo test *`/`cargo check *`, not `cargo run *`/`cargo doc *`/...), so
/// an otherwise-legitimate in-family command still hit a silent, final
/// denial. The narrow per-subcommand entries are replaced with whole
/// `Bash(<tool> *)` families (`git *`, `gh *`, `cargo *`, `npm *`, `npx *`,
/// `node *`, `python *`, `python3 *`, `pip *`, `go *`, `dotnet *`, `make *`,
/// `gradle *`, `mvn *`, `pytest *`) plus a set of read-only shell
/// utilities -- the deny list, not per-verb narrowing, is what still keeps
/// each family's destructive half blocked (`git clean *`, `git push
/// --delete *`, `gh repo delete *`, `gh release delete *`, `gh auth *`,
/// `cargo publish *`, `npm publish *`, added to `SHIPPED_POSTURE_DENY`
/// alongside the pre-existing force-push/reset/rebase/curl/wget/sudo/su/
/// security entries -- deny still wins, verified live in fix round 2).
///
/// Issue #224 replaces issue #98's former blanket `zirv *` family with rules
/// derived from `utils::RESERVED_COMMANDS` in `safety::builtin_allow`.
/// Zirv's own built-ins remain usable under `dontAsk`, while repo-defined
/// `zirv <script>` invocations return to the unmatched-command gate.
///
/// Also added: `Read(~/.claude/**)`/`Edit(~/.claude/projects/**)` (inspect
/// the harness's own settings/memory, and write Claude Code's own
/// auto-memory, which lives under `~/.claude/projects/<slug>/memory/`);
/// `Read(~/.zirv/**)` (inspect the operator layer -- `Edit(~/.zirv/**)` is
/// denied below, since a session must never widen its own posture);
/// `WebFetch`/`WebSearch` (bare tool rules, no `Bash(...)` wrapper); and two
/// scratchpad rules computed at launch from the real `std::env::temp_dir()`
/// rather than baked into this `&'static` list -- see [`scratchpad_rules`].
///
/// **Fix round 6 (2026-09-16, docs/superpowers/2026-09-16-builtin-safe-
/// command-policy.md, Change 2): the worker capability list.** This list is
/// also what a headless `zirv ctx agent`/`exec`/`loop` worker can do
/// unattended -- unlike an interactive session, it has no unmatched-command
/// fallback to lean on (`headless_default = ask`, unanswerable), so a
/// command absent here silently blocks a worker even though the identical
/// command is already `Allow` interactively. `glab *`, `gitlab-ci-local *`,
/// `php *`; read-only `kubectl get/logs/describe/config *` (never a bare
/// `kubectl *` -- `apply`/`delete` stay off); `docker exec *`/`kubectl exec
/// *` (safe only because the inner command is now analysed -- see the two
/// entries' own comments and the spec's Change 3); and the everyday-tool
/// gap (`sed`, `awk`, `jq`, `mkdir`, `touch`, `cp`, `mv`, `stat`, `df`, `du`,
/// `ps`, `printf`, `date`, `basename`, `dirname`, `xargs`, `tee`, `mktemp`,
/// `realpath`), plus the fixed macOS SSH-agent environment lookup
/// (`launchctl getenv` and `export SSH_AUTH_SOCK=...`).
///
/// **Fix round 7 (2026-09-20): the remaining harmless built-ins.** `Grep`,
/// `Glob`, `AskUserQuestion`, `TodoWrite`, `NotebookRead` and `TaskOutput`
/// are added as whole-tool allow entries alongside `WebFetch`/`WebSearch`
/// above -- each is either read-only (`Grep`, `Glob`, `NotebookRead`,
/// `TaskOutput`) or pure in-conversation UI state with no filesystem or
/// process effect (`AskUserQuestion`, `TodoWrite`), so none of them can
/// touch the machine, the repo or production. `Monitor`, `Skill` and
/// `Agent`/`Task` are deliberately excluded even though they also showed up
/// in `permission-prompts.jsonl`: `Monitor` can run and stream arbitrary
/// shell commands, and `Skill`/`Agent` can themselves invoke
/// `Bash`/`Write`/`Edit`, so all three must stay behind the same gate as
/// `Bash` rather than being pre-approved as leaf tools.
pub const SHIPPED_POSTURE_ALLOW: &[(&str, &str)] = &[
    ("Read(./**)", "read anything inside the workspace"),
    (
        "Edit(./**)",
        "create or modify files inside the workspace (covers both the Write and Edit tools)",
    ),
    (
        "Read(~/.claude/**)",
        "inspect the harness's own settings and memory",
    ),
    (
        "Edit(~/.claude/projects/**)",
        "Claude Code's own auto-memory lives under ~/.claude/projects/<slug>/memory/",
    ),
    (
        "Read(~/.zirv/**)",
        "inspect the operator layer (editing it is denied below)",
    ),
    ("WebFetch", "fetch a URL's contents, read-only"),
    ("WebSearch", "search the web, read-only"),
    // Fix round 7 (2026-09-20): whole-tool allow entries for the remaining
    // built-ins that cannot touch the machine, the repo or production --
    // each is either read-only or purely in-conversation UI state, so
    // gating them behind a prompt bought nothing (permission-prompts.jsonl:
    // Grep 14, Glob 1, AskUserQuestion 7 of the 345 logged prompts were
    // exactly this). `Monitor`, `Skill` and `Agent`/`Task` are deliberately
    // NOT here even though they also showed up in that log: `Monitor` can
    // run and stream arbitrary shell commands, and `Skill`/`Agent` can
    // themselves invoke `Bash`/`Write`/`Edit`, so all three stay gated by
    // the same posture that gates `Bash` itself rather than being
    // pre-approved as if they were leaf tools.
    ("Grep", "search file contents, read-only"),
    ("Glob", "match file paths by pattern, read-only"),
    (
        "AskUserQuestion",
        "ask the operator a clarifying question; no side effects",
    ),
    (
        "TodoWrite",
        "update the in-conversation todo list; conversation state, not a file or shell write",
    ),
    (
        "NotebookRead",
        "read a Jupyter notebook's cells and outputs, read-only",
    ),
    ("TaskOutput", "read a delegated agent's output; read-only"),
    // Whole toolchain families (2026-08-23, fix round 4, issue #104) -- see
    // this constant's own doc comment for why the narrower per-subcommand
    // entries these replace were still inert-by-omission on anything else
    // in the same family.
    (
        "Bash(git *)",
        "the full git command family; force-push, hard reset, rebase, filter-branch and clean are denied below and win",
    ),
    (
        "Bash(gh *)",
        "the GitHub CLI; repo/release delete and auth are denied below and win",
    ),
    (
        "Bash(cargo *)",
        "the Rust toolchain; publish is denied below",
    ),
    (
        "Bash(npm *)",
        "the Node/npm toolchain; publish is denied below",
    ),
    (
        "Bash(npx *)",
        "run a Node package binary with no separate install step",
    ),
    ("Bash(node *)", "run a Node script directly"),
    ("Bash(python *)", "the Python toolchain"),
    (
        "Bash(python3 *)",
        "the Python toolchain, explicit-version spelling",
    ),
    ("Bash(pip *)", "install or manage Python packages"),
    ("Bash(go *)", "the Go toolchain"),
    ("Bash(dotnet *)", "the .NET toolchain"),
    ("Bash(make *)", "a Makefile-based toolchain"),
    ("Bash(gradle *)", "the Java/Kotlin/Gradle toolchain"),
    ("Bash(mvn *)", "the Java/Maven toolchain"),
    ("Bash(pytest *)", "test with the Python toolchain"),
    // Read-only shell utilities.
    ("Bash(ls *)", "list directory contents, read-only"),
    ("Bash(grep *)", "search file contents, read-only"),
    ("Bash(rg *)", "search file contents, read-only"),
    ("Bash(cat *)", "read file contents, read-only"),
    ("Bash(head *)", "read the start of a file, read-only"),
    ("Bash(tail *)", "read the end of a file, read-only"),
    ("Bash(wc *)", "count lines, words or bytes, read-only"),
    (
        "Bash(find *)",
        "search for files by name; read-only DENIED into -delete/-exec/-ok by the entries below",
    ),
    ("Bash(echo *)", "print text, read-only, no side effects"),
    ("Bash(pwd)", "print the working directory, read-only"),
    ("Bash(which *)", "locate a command on PATH, read-only"),
    (
        "Bash(where *)",
        "locate a command on PATH, read-only (Windows spelling)",
    ),
    (
        "Bash(launchctl getenv SSH_AUTH_SOCK)",
        "read the SSH agent socket from launchd's environment",
    ),
    (
        "Bash(export SSH_AUTH_SOCK=*)",
        "point this shell at the SSH agent socket; command substitutions are analysed separately",
    ),
    ("Bash(diff *)", "compare files, read-only"),
    ("Bash(sort *)", "sort input lines, read-only"),
    ("Bash(uniq *)", "filter duplicate lines, read-only"),
    ("Bash(tr *)", "translate or delete characters, read-only"),
    ("Bash(cut *)", "extract fields from input, read-only"),
    // Moved out of SHIPPED_POSTURE_DENY (2026-08-24, primary acceptance
    // criterion): fetching a URL is everyday dev work -- checking an API,
    // downloading a fixture -- and denying the tool wholesale is exactly the
    // over-blocking this round exists to remove. The real danger, a download
    // piped straight into a shell, is denied on its own below.
    (
        "Bash(curl *)",
        "fetch a URL; piping into a shell is denied below",
    ),
    (
        "Bash(wget *)",
        "fetch a URL; piping into a shell is denied below",
    ),
    // Fix round 6 (2026-09-16, issue tracked in docs/superpowers/2026-09-16-
    // builtin-safe-command-policy.md, Change 2) -- see this constant's own
    // doc comment for the "worker capability list" framing.
    (
        "Bash(glab *)",
        "the GitLab CLI; resolves a real asymmetry with `Bash(gh *)` above -- \
         glab was absent from this internal policy while the Claude-adapter- \
         native PROMPT_FREE_COMMAND_FAMILIES (adapters/claude.rs, issue #329) \
         already grants `glab *` prompt-free treatment, so the two layers \
         disagreed on the same command; destructive glab forms are still \
         denied by the semantic classifier ahead of this family",
    ),
    (
        "Bash(gitlab-ci-local *)",
        "run this project's CI jobs locally against the GitLab CI config",
    ),
    ("Bash(php *)", "the PHP toolchain"),
    // kubectl READ verbs only -- deliberately not a bare `Bash(kubectl *)`:
    // `apply`/`delete`/`exec` must not inherit this blanket allow.
    (
        "Bash(kubectl get *)",
        "inspect cluster resources, read-only",
    ),
    ("Bash(kubectl logs *)", "read a pod's logs, read-only"),
    (
        "Bash(kubectl describe *)",
        "inspect a resource in detail, read-only",
    ),
    (
        "Bash(kubectl config *)",
        "inspect or switch kubeconfig context, read-only",
    ),
    // Container exec, per an explicit operator decision (spec Change 3):
    // safe to allow ONLY because the inner command run inside the container/
    // pod is decoded and analysed like any other nested command, not treated
    // as one opaque string -- see the wrapper-decoding work tracked
    // alongside this change. Without that analysis these two entries would
    // let `docker exec db rm -rf /tmp/data` through unexamined.
    (
        "Bash(docker exec *)",
        "exec into a running container; the inner command is analysed, not opaque",
    ),
    (
        "Bash(kubectl exec *)",
        "exec into a running pod; the inner command is analysed, not opaque",
    ),
    // Everyday tools absent today -- the same "worker capability list" gap
    // as `glab`/`kubectl` above, just for common shell utilities rather than
    // a named toolchain.
    ("Bash(sed *)", "stream-edit text, including in-place edits"),
    ("Bash(awk *)", "pattern-scan and process text"),
    ("Bash(jq *)", "query and transform JSON"),
    ("Bash(mkdir *)", "create a directory"),
    ("Bash(touch *)", "create a file or update its timestamp"),
    ("Bash(cp *)", "copy files"),
    ("Bash(mv *)", "move or rename files"),
    ("Bash(stat *)", "read file metadata, read-only"),
    ("Bash(df *)", "report filesystem disk usage, read-only"),
    ("Bash(du *)", "report directory disk usage, read-only"),
    ("Bash(ps *)", "list running processes, read-only"),
    ("Bash(printf *)", "print formatted text, read-only"),
    ("Bash(date *)", "print or compute a date, read-only"),
    ("Bash(basename *)", "strip a path down to its filename"),
    ("Bash(dirname *)", "strip a path down to its directory"),
    (
        "Bash(xargs *)",
        "build and run commands from input; the launched command is analysed on its own",
    ),
    ("Bash(tee *)", "write standard input to a file and stdout"),
    ("Bash(mktemp *)", "create a temporary file or directory"),
    ("Bash(realpath *)", "resolve a path, read-only"),
];

/// Projects the operator's scratchpad temp directory into the two claude
/// permission rules that make it usable under `dontAsk` -- computed at
/// launch (2026-08-23, issue #104) inside `ClaudeAdapter::
/// default_sandbox_args` rather than baked into [`SHIPPED_POSTURE_ALLOW`],
/// since the path is per-machine and that constant has to stay `&'static`.
///
/// Claude Code's absolute-path rule form is a *doubled* leading slash
/// (`//<path>`, the same convention `SHIPPED_POSTURE_ALLOW`'s own doc
/// comment cites live findings against). `temp_dir` is normalized to
/// forward slashes, any trailing slash is removed, then **one** leading
/// slash (if the path already had one, e.g. a Unix absolute path) is
/// stripped before the `//` prefix is added -- so the result always has
/// exactly two leading slashes, never three. A Windows path with no leading
/// slash of its own (a drive letter) is unaffected by the strip:
/// `C:\Users\x\AppData\Local\Temp\` becomes
/// `//C:/Users/x/AppData/Local/Temp/claude/**`; a Unix `/tmp` base with UID
/// 501 becomes `//tmp/claude-501/**`, not `///tmp/claude-501/**`.
pub(crate) fn scratchpad_rules(temp_dir: &Path) -> Vec<String> {
    scratchpad_rules_from_roots(&scratchpad_roots(temp_dir))
}

/// Claude Code's absolute-path permission-rule form, extracted so every
/// caller that projects a real filesystem root into an `Edit`/`Read` rule
/// (this module's own scratchpad rules, and issue #504's `--add-dir`
/// widening in `adapters::claude::add_dir_edit_read_rules`) spells it
/// identically rather than duplicating the normalization: forward slashes,
/// no trailing slash, and a DOUBLED leading slash (`//<path>`, verified live
/// findings cited in this function's own doc comment above). `root` may
/// carry either separator -- a raw OS-native path (backslashes on Windows,
/// as `ClaudeAdapter::grant_path` produces) is normalized here, not just an
/// already-forward-slash scratchpad root -- and the result always has
/// exactly two leading slashes, never one or three.
pub(crate) fn doubled_slash_rule_base(root: &str) -> String {
    let normalized = root.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let stripped = normalized.strip_prefix('/').unwrap_or(normalized);
    format!("//{stripped}")
}

fn scratchpad_rules_from_roots(roots: &[String]) -> Vec<String> {
    roots
        .iter()
        .flat_map(|root| {
            let base = format!("{}/**", doubled_slash_rule_base(root));
            [format!("Read({base})"), format!("Edit({base})")]
        })
        .collect()
}

/// The harness scratchpad roots from `CLAUDE_CODE_TMPDIR`, `/tmp` on Unix,
/// or the platform temp base on Windows, normalized with no trailing separator -- the
/// single source both [`scratchpad_rules`] and the safety hook's confined-
/// write classifier (`safety::scratchpad_write_roots`) derive from.
pub(crate) fn scratchpad_roots(temp_dir: &Path) -> Vec<String> {
    let claude_tmpdir = std::env::var_os("CLAUDE_CODE_TMPDIR").map(PathBuf::from);
    #[cfg(unix)]
    {
        // SAFETY: `getuid` takes no arguments and has no failure mode.
        scratchpad_roots_for(
            temp_dir,
            Some(unsafe { libc::getuid() }),
            claude_tmpdir.as_deref(),
        )
    }
    #[cfg(not(unix))]
    {
        scratchpad_roots_for(temp_dir, None, claude_tmpdir.as_deref())
    }
}

/// Builds the stable scratchpad-root set for a supplied platform UID and
/// optional Claude-specific temp base.
pub(crate) fn scratchpad_roots_for(
    temp_dir: &Path,
    uid: Option<u32>,
    claude_tmpdir: Option<&Path>,
) -> Vec<String> {
    let normalized_temp_dir = temp_dir.to_string_lossy().replace('\\', "/");
    let normalized_temp_dir = normalized_temp_dir.trim_end_matches('/');
    let Some(uid) = uid else {
        return vec![format!("{normalized_temp_dir}/claude")];
    };

    let normalized_base = claude_tmpdir
        .unwrap_or_else(|| Path::new("/tmp"))
        .to_string_lossy()
        .replace('\\', "/");
    let normalized_base = normalized_base.trim_end_matches('/');
    let suffix = format!("claude-{uid}");
    let mut roots = vec![
        if normalized_base.rsplit('/').next() == Some(suffix.as_str()) {
            normalized_base.to_string()
        } else {
            format!("{normalized_base}/{suffix}")
        },
    ];
    if normalized_temp_dir.rsplit('/').next() == Some(suffix.as_str()) {
        let normalized_temp_dir = normalized_temp_dir.to_string();
        if !roots.contains(&normalized_temp_dir) {
            roots.push(normalized_temp_dir);
        }
    }
    #[cfg(target_os = "macos")]
    for root in roots.clone() {
        let twin = if root.starts_with("/tmp/") {
            Some(format!("/private{root}"))
        } else {
            root.strip_prefix("/private/tmp/")
                .map(|tail| format!("/tmp/{tail}"))
        };
        if let Some(twin) = twin
            && !roots.contains(&twin)
        {
            roots.push(twin);
        }
    }
    roots
}

/// The destructive families this posture denies regardless of anything on
/// [`SHIPPED_POSTURE_ALLOW`] -- verified live to win over a broader,
/// overlapping allow entry (see that constant's own doc comment).
///
/// **Fix round 4 additions (2026-08-23, issue #104):** `Edit(~/.zirv/**)`
/// (a session must never widen its own posture -- `Read(~/.zirv/**)` is
/// allowed above, editing it is not) and `Read(~/.claude/.credentials.json)`
/// (the harness's own stored OAuth credentials, alongside the harness dirs
/// newly allowed above) -- both non-`Bash` entries, declared first so the
/// claude projection can prepend them the same way it prepends
/// [`SHIPPED_POSTURE_ALLOW`]'s own non-`Bash` entries, see `ClaudeAdapter::
/// default_sandbox_args`. Plus the destructive halves of the toolchain
/// families [`SHIPPED_POSTURE_ALLOW`] widened to whole `Bash(<tool> *)`
/// entries: `cargo publish *`/`npm publish *` (irreversible), `gh repo
/// delete *`/`gh release delete *`/`gh auth *`, `git clean *`, `git push
/// --delete *`. A trailing `" *"` denies the bare invocation too, not only
/// one carrying flags (issue #106's `glob_match` fix -- a claude `Bash(<x>
/// *)` rule is documented to match the bare `<x>` as well).
///
/// **Fix round 5 (2026-08-23, issue #111): argument-reordering and
/// sibling-utility bypasses.** PR #107's review found the round-4 `git
/// push`/`git reset` entries were flag-anchored (`Bash(git push --force *)`)
/// and so matched only when the dangerous flag came first -- `git push
/// origin --force` slipped through untouched, as did the short-flag
/// spellings (`-f`, `-d`), an empty-src refspec delete (`git push origin
/// :branch`), and a force-refspec push (`git push origin +branch`). Those
/// entries are replaced with mid-string-wildcard patterns below (`glob_
/// match` already supports `*` anywhere, not only as a suffix). `find`'s
/// own `-delete`/`-exec`/`-ok` actions, and the credential-path reads
/// `head`/`tail`/`diff` can perform just as well as the already-denied
/// `cat`, are closed the same way, plus three `gh` escapes (`gh api -X
/// DELETE`, `gh secret`, `gh codespace ssh`). **With arbitrary-code
/// toolchains (`python *`, `node *`, ...) allowed by
/// [`SHIPPED_POSTURE_ALLOW`], this list is a tripwire for named
/// destructive/credential command families, not a security boundary** -- a
/// session can always reach the same effect through an interpreter one-liner
/// this list cannot enumerate in advance; the README already frames the
/// shipped posture as an honest partial, and this round narrows the gap
/// without pretending to close it.
pub const SHIPPED_POSTURE_DENY: &[(&str, &str)] = &[
    (
        "Edit(~/.zirv/**)",
        "a session must never widen its own posture",
    ),
    (
        "Read(~/.claude/.credentials.json)",
        "the harness's own stored OAuth credentials",
    ),
    // Self-destructive (2026-08-24): this session itself runs under zirv, so
    // killing a zirv process kills the supervisor that would have asked the
    // question. `evaluate_single` walks the whole deny list before it looks
    // at ask at all, so these beat the broad `taskkill *`/`rm -rf *` entries
    // in SHIPPED_POSTURE_ASK with no ordering rule needed.
    ("Bash(taskkill*zirv*)", "kills the supervising zirv session"),
    (
        "Bash(Stop-Process*zirv*)",
        "kills the supervising zirv session, PowerShell spelling",
    ),
    ("Bash(pkill*zirv*)", "kills the supervising zirv session"),
    ("Bash(killall*zirv*)", "kills the supervising zirv session"),
    (
        "Bash(rm -rf*zirv*)",
        "destroys zirv's own state or operator layer",
    ),
    (
        "Bash(rm -fr*zirv*)",
        "destroys zirv's own state or operator layer, flag-order variant",
    ),
    (
        "Bash(Remove-Item*zirv*)",
        "destroys zirv's own state or operator layer, PowerShell spelling",
    ),
    // The actual danger `curl`/`wget` were denied wholesale for, now denied
    // precisely instead: a remote download executed as a shell script. These
    // are whole-string patterns, matched against the raw command -- which
    // `evaluate` always checks as its first candidate.
    (
        "Bash(* | sh)",
        "a remote download executed as a shell script",
    ),
    (
        "Bash(* | bash)",
        "a remote download executed as a shell script",
    ),
    (
        "Bash(* | zsh)",
        "a remote download executed as a shell script",
    ),
    (
        "Bash(*| sh)",
        "a remote download executed as a shell script, no space before the pipe",
    ),
    (
        "Bash(*| bash)",
        "a remote download executed as a shell script, no space before the pipe",
    ),
    ("Bash(sudo *)", "privilege escalation"),
    ("Bash(doas *)", "privilege escalation"),
    ("Bash(su *)", "privilege escalation"),
    (
        "Bash(security *)",
        "macOS keychain CLI; reads stored credentials",
    ),
    // Credential-path reads (2026-08-22, fix round 3): the allow list never
    // grants a broad `cat`/`Bash`, so these were already denied by
    // omission -- explicit here so the guarantee does not rest on that
    // remaining true as the allow list grows. A mid-string wildcard was
    // verified live to be honored (`Bash(cat *.aws*)` denied `cat
    // .aws/credentials`), not assumed from the prefix-only doc example.
    (
        "Bash(cat *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(cat *.aws*)", "reads AWS credential files"),
    ("Bash(cat *.ssh*)", "reads SSH private keys"),
    ("Bash(cat *.netrc*)", "reads stored HTTP credentials"),
    // Credential-path reads, head/tail/diff parity (2026-08-23, issue
    // #111): `head`/`tail`/`diff` can read the same credential paths `cat`
    // can, and were not covered by the `cat`-anchored entries above.
    (
        "Bash(head *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(head *.aws*)", "reads AWS credential files"),
    ("Bash(head *.ssh*)", "reads SSH private keys"),
    ("Bash(head *.netrc*)", "reads stored HTTP credentials"),
    (
        "Bash(tail *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(tail *.aws*)", "reads AWS credential files"),
    ("Bash(tail *.ssh*)", "reads SSH private keys"),
    ("Bash(tail *.netrc*)", "reads stored HTTP credentials"),
    (
        "Bash(diff *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(diff *.aws*)", "reads AWS credential files"),
    ("Bash(diff *.ssh*)", "reads SSH private keys"),
    ("Bash(diff *.netrc*)", "reads stored HTTP credentials"),
    ("Bash(cargo publish *)", "publishes a crate; irreversible"),
    ("Bash(npm publish *)", "publishes a package; irreversible"),
    (
        "Bash(gh repo delete *)",
        "irreversibly deletes a GitHub repository",
    ),
    (
        "Bash(gh release delete *)",
        "irreversibly deletes a GitHub release",
    ),
    (
        "Bash(gh auth *)",
        "changes or reveals the operator's own GitHub authentication",
    ),
    // gh escapes (2026-08-23, issue #111).
    (
        "Bash(gh api*DELETE*)",
        "covers both -X DELETE and --method DELETE",
    ),
    ("Bash(gh secret *)", "reads or writes repository secrets"),
    ("Bash(gh codespace ssh*)", "opens a shell into a codespace"),
];

/// The short, closed list of families zirv's shipped posture wants a HUMAN to
/// see before they run (2026-08-24, cross-harness permissions design).
///
/// **This list is deliberately narrow, and adding to it is a product
/// decision, not a hardening reflex.** The primary acceptance criterion is
/// that an everyday dev command -- and a command zirv has never seen -- never
/// prompts. Every entry here is a prompt an operator will actually be
/// interrupted by, so the bar for membership is "genuinely dangerous and
/// hard to undo", not "mutates something". `cargo build`, `npm install`,
/// `git commit`, `mkdir`, an in-repo file write, a plain `curl` and an
/// unrecognised tool are all `Allow`, and must stay that way -- pinned by
/// `the_product_requirement_no_everyday_or_novel_command_ever_prompts` in
/// `safety.rs`.
///
/// **Split from [`SHIPPED_POSTURE_DENY`] by reversibility, not by danger.**
/// `git push --force` is recoverable from a reflog and `rm -rf ./target`
/// from a rebuild, so both ask. `cargo publish` is irreversible and
/// `cat ~/.ssh/id_rsa` has already leaked by the time anyone sees the
/// prompt, so both stay denied.
///
/// **Deny still wins**: `safety::evaluate_single` walks deny before ask, so
/// the specific `Bash(taskkill*zirv*)` deny beats the broad
/// `Bash(taskkill *)` ask here with no ordering rule of its own.
///
/// Projected differently per launch mode: claude's INTERACTIVE argv leaves
/// these off `--allowedTools`, so the safety hook's `"ask"` decision is what
/// prompts on them; claude's HEADLESS argv folds them into
/// `--disallowedTools` alongside the deny set, since nobody is present to
/// answer (see `ClaudeAdapter::default_sandbox_args`).
pub const SHIPPED_POSTURE_ASK: &[(&str, &str)] = &[
    ("Bash(rm -rf *)", "recursive force-delete"),
    (
        "Bash(rm -fr *)",
        "recursive force-delete, flag-order variant",
    ),
    (
        "Bash(git push*--force*)",
        "force-push (covers --force-with-lease too), any argument position",
    ),
    (
        "Bash(git push* -f *)",
        "force-push, short-flag form, followed by more arguments",
    ),
    (
        "Bash(git push* -f)",
        "force-push, short-flag form, as the final argument",
    ),
    (
        "Bash(git push*--delete*)",
        "deletes a remote branch, any argument position",
    ),
    (
        "Bash(git push* -d *)",
        "deletes a remote branch, short-flag form, followed by more arguments",
    ),
    (
        "Bash(git push* -d)",
        "deletes a remote branch, short-flag form, as the final argument",
    ),
    (
        "Bash(git push* :*)",
        "empty-src refspec delete (git push origin :branch)",
    ),
    (
        "Bash(git push* +*)",
        "force-refspec push (git push origin +branch)",
    ),
    (
        "Bash(git reset*--hard*)",
        "destroys uncommitted work and can discard commits, any argument position",
    ),
    // Issue #306: `rebase` and `clean` no longer have a blanket glob entry
    // here -- unlike a bare glob, both need to inspect ARGUMENTS to tell a
    // genuinely dangerous invocation from routine, local, agent-scoped work
    // (a non-interactive `rebase`; a pathed `clean -f...` that names an
    // explicit target and does not also remove gitignored files). That
    // per-argument judgment is `safety::is_destructive_vcs_action`'s job,
    // not a glob's -- it already owns the identical judgment for `checkout`/
    // `restore`/`worktree remove`/`branch -D`/`stash drop`/`clear`/`reflog
    // expire`/`delete`/`gc --prune`, none of which have a blanket entry
    // here either. `filter-branch` keeps its own blanket entry: it rewrites
    // history unconditionally, with no narrower form to allow.
    ("Bash(git filter-branch *)", "rewrites commit history"),
    // These three glob entries name the most common shapes literally; the
    // general case -- ANY `find -exec`/`-ok`/`-execdir`/`-okdir` action that
    // is not on a small proven-safe allow-list (`find -exec sh -c ...`,
    // `find -exec chmod -R 777 ...`, `-ok rm ...`, ...) -- is caught by
    // `safety::apply_find_exec_outcome` (`is_risky_find_exec`) instead: an
    // ask-unless-proven-safe semantic gate, not another glob to keep
    // enumerating. `find -exec grep`/`-exec sed -n` are everyday read-only
    // work and must not prompt, which is why a blanket `find*-exec*` glob
    // entry is still not carried over here.
    ("Bash(find*-delete*)", "find's own delete action"),
    (
        "Bash(find*-exec rm*)",
        "find's exec action invoking a delete",
    ),
    (
        "Bash(find*-exec*rm -rf*)",
        "find's exec action invoking a recursive force-delete",
    ),
    // Process termination. The zirv-specific spellings are DENIED above and
    // win, since deny is walked first.
    ("Bash(taskkill *)", "terminates a running process"),
    (
        "Bash(Stop-Process *)",
        "terminates a running process, PowerShell spelling",
    ),
    ("Bash(pkill *)", "terminates running processes by name"),
    ("Bash(killall *)", "terminates running processes by name"),
    (
        "Bash(Remove-Item*-Recurse*)",
        "recursive delete, PowerShell spelling",
    ),
    // Raw device and partition tools.
    (
        "Bash(dd *)",
        "writes raw blocks; can destroy a whole device",
    ),
    ("Bash(mkfs*)", "formats a filesystem; destroys its contents"),
    ("Bash(mkswap *)", "reformats a device as swap"),
    ("Bash(diskpart*)", "Windows disk partitioning tool"),
    ("Bash(fdisk *)", "disk partitioning tool"),
    ("Bash(format *)", "formats a volume; destroys its contents"),
    // Registry MUTATION only -- `reg query` is read-only and must not prompt.
    (
        "Bash(reg delete*)",
        "deletes a Windows registry key or value",
    ),
    ("Bash(reg add*)", "writes a Windows registry key or value"),
    ("Bash(reg import*)", "bulk-writes the Windows registry"),
    ("Bash(shutdown *)", "powers off or restarts the machine"),
    ("Bash(reboot*)", "restarts the machine"),
];

/// `AgentAdapter::read_only_args`/`interactive_read_only_args` for a
/// registered adapter name, without requiring that adapter to be enabled or
/// ready -- the same static-fact lookup through `ADAPTERS` that
/// `provider_for_agent_name` does. `None` for an unknown name, so a caller
/// that must not launch an unpinned child can refuse rather than guess an
/// empty restriction.
///
/// `mode` picks which of the two floors applies: bug fix (2026-09-06),
/// `dash::worker_pane_extra_args` used to call this with no mode at all and
/// always got the `exec`-only floor, which carried codex's `--ignore-rules`/
/// `--ignore-user-config` onto an interactive pane launch that rejects both
/// -- see `AgentAdapter::interactive_read_only_args`'s own doc comment.
pub fn read_only_args_for_agent_name(name: &str, mode: LaunchMode) -> Option<Vec<String>> {
    ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .map(|(_, ctor)| {
            let adapter = ctor(None);
            // Issue #89: the workflow reviewer's own choke point for
            // resolving a read-only pin by name -- a sibling call site to
            // the ones production callers make directly around `handoff::
            // run_model` for the distiller role. `chrome.events` is not
            // known at this call site (no `CtxConfig` in hand), so this
            // defaults to enabled, matching this function's own pre-
            // existing "no config, no repo" shape; `reviewer_args`'s own
            // caller may still be running under `ZIRV_CTX_QUIET`, which
            // `Announcer` itself does not re-check here -- see the
            // documented residual on `announce_sandbox_residual_once`.
            announce_sandbox_residual_once(adapter.as_ref(), true);
            if mode.is_interactive() {
                adapter.interactive_read_only_args()
            } else {
                adapter.read_only_args()
            }
        })
}

/// Apply a worker's read-only floor at the launch composition seam.
/// Codex's legacy sandbox selection must not override the named mail profile.
/// Other adapters retain their own floor argv.
pub fn extend_read_only_args(
    adapter: &(impl AgentAdapter + ?Sized),
    args: &mut Vec<String>,
    mode: LaunchMode,
) {
    let mut floor = if mode.is_interactive() {
        adapter.interactive_read_only_args()
    } else {
        adapter.read_only_args()
    };
    if adapter.name() == "codex" {
        let uses_profile = floor
            .iter()
            .any(|arg| arg.starts_with("default_permissions="));
        let mut replaced = false;
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if arg == "--sandbox" || arg == "-s" {
                if uses_profile {
                    args.drain(index..(index + 2).min(args.len()));
                    continue;
                }
                args[index] = "--sandbox".to_string();
                if index + 1 == args.len() {
                    args.push("read-only".to_string());
                } else {
                    args[index + 1] = "read-only".to_string();
                }
                replaced = true;
                index += 1;
            } else if arg.starts_with("--sandbox=") || (arg.starts_with("-s") && arg.len() > 2) {
                if uses_profile {
                    args.remove(index);
                    continue;
                }
                args.splice(
                    index..=index,
                    ["--sandbox".to_string(), "read-only".to_string()],
                );
                replaced = true;
                index += 1;
            }
            index += 1;
        }
        if replaced {
            floor.drain(..2);
        }
        let mut index = 0;
        while index + 1 < floor.len() {
            if floor[index] == "-c" && args.windows(2).any(|pair| pair == &floor[index..index + 2])
            {
                floor.drain(index..index + 2);
            } else {
                index += 1;
            }
        }
        // A workflow reviewer can already carry the exec-only floor.
        floor.retain(|arg| {
            !matches!(arg.as_str(), "--ignore-rules" | "--ignore-user-config")
                || !args.contains(arg)
        });
    } else {
        floor.retain(|arg| !args.contains(arg));
    }
    args.extend(floor);
}

/// The argv `policy_launch_args` prepends ahead of an operator's own
/// trailing flags, at every real-launch seam this codebase builds
/// (`agent.rs::worker_launch_flags`, `exec.rs`, `run_loop.rs`, `wrap.rs`,
/// `chat.rs::dash_orchestrator_pane`, `dash::mod::fulfill_spawn_request`,
/// `handover.rs::resolve_swap_launch`) -- the one function all seven call,
/// so "operator's own choice always wins" and the shipped-default posture
/// can never drift between seams. Every one of those seams also already
/// knows or derives its own compiled [`super::super::prompt::PromptRole`] (see
/// each caller's own `role`/`spawnreq::role_of`/`Pane::role` source), which
/// is why `role` threads through here rather than needing a new lookup.
///
/// `Vec::new()` when `flags_pin_policy(flags)`: the operator's own explicit
/// flag wins outright, nothing of zirv's own is prepended at all. Otherwise:
/// `adapter.default_sandbox_args()` when `cfg.sandbox.enabled` (the shipped
/// default -- see that method's own doc comment for the exact posture),
/// followed by `adapter.policy_args(&cfg.policy, mode)` for any *additional*
/// restriction an explicit `[policy]` `Deny` stance asks for on top of the
/// baseline. Codex's restrictive policy replaces the baseline because its
/// sandbox and approval options reject duplicate occurrences.
///
/// `role` is the compiled prompt role this launch actually gets, forwarded
/// to [`AgentAdapter::plugin_dir_args`] alone (see that method's own doc
/// comment, skill-listing overhead fix) -- it changes nothing else this
/// function computes.
pub fn policy_launch_args(
    cfg: &CtxConfig,
    adapter: &(impl AgentAdapter + ?Sized),
    flags: &[String],
    mode: LaunchMode,
    role: super::super::prompt::PromptRole,
) -> Vec<String> {
    policy_launch_args_for_surface(cfg, adapter, flags, mode, mode, role)
}

/// The general form of [`policy_launch_args`] above, for the one caller
/// whose approval-permissiveness signal and actual CLI launch surface can
/// diverge: a dashboard pane (`dash::worker_pane_extra_args`, review round,
/// issue #326) always launches through [`AgentAdapter::interactive_cmd`] --
/// `SpawnRequest::interactive` never gates that choice, only whether this
/// pane gets the permissive interactive approval posture or the fail-closed
/// headless one (issue #230 finding 10). Every OTHER real-launch seam this
/// module's own doc comment above lists passes one `mode` that already
/// equals its real launch surface (`agent.rs`/`exec.rs`/`run_loop.rs` are
/// always headless-launched; `chat.rs`/`wrap.rs`/`handover.rs` always
/// interactive-launched), so `policy_launch_args` above still passes the
/// same `mode` to both halves for them -- this split only matters for the
/// one caller where it doesn't hold.
///
/// `approval_mode` feeds `default_sandbox_args` (and nothing else): its own
/// argv is safe under either `LaunchMode` value regardless of the REAL
/// surface -- `--ask-for-approval`/`--approve-for-me`/`-c approval_policy=`
/// are all verified present on both codex's top-level interactive launch
/// and `codex exec` (see `CodexAdapter::approval_suppression_args`'s own
/// doc comment) -- so it is safe, and correct, to keep it driven by
/// `SpawnRequest::interactive`'s fail-closed signal.
///
/// `surface_mode` feeds `policy_args`, because THAT is where a genuinely
/// surface-unsafe choice lives: `CodexAdapter::policy_args`'s Deny-stance
/// branch picks between `read_only_args()` (exec-only `--ignore-rules
/// --ignore-user-config`, safe only on `codex exec`) and
/// `interactive_read_only_args()` (never those two, safe on codex's
/// top-level interactive launch too) -- see `AgentAdapter::
/// interactive_read_only_args`'s own doc comment. Passing a `Headless`
/// approval-permissiveness signal in as `surface_mode` too, for a
/// `SpawnRequest { interactive: false, .. }` that is nonetheless fulfilled
/// as a real interactive pane (an ordinary `zirv ctx agent` dispatch, which
/// never claims `interactive` since it cannot vouch a human is watching the
/// dashboard that might pick the request up -- `agent.rs`'s own doc comment
/// on that field), reproduced the exact "pane exited with code 2" crash
/// this whole fix exists to close, just through the canonical `[policy]`
/// `Deny` stance instead of `--mode read-only`'s own floor.
pub fn policy_launch_args_for_surface(
    cfg: &CtxConfig,
    adapter: &(impl AgentAdapter + ?Sized),
    flags: &[String],
    approval_mode: LaunchMode,
    surface_mode: LaunchMode,
    role: super::super::prompt::PromptRole,
) -> Vec<String> {
    if flags_pin_policy(flags) {
        return adapter.plugin_dir_args(flags, role);
    }
    let policy = adapter.policy_args(&cfg.policy, surface_mode);
    // Codex's restrictive policy already supplies sandbox and approval.
    // Neither CLI option accepts a second occurrence from the baseline.
    let policy_supplies_sandbox = adapter.name() == "codex" && flags_pin_policy(&policy);
    let mut out = if cfg.sandbox.enabled && !policy_supplies_sandbox {
        adapter.default_sandbox_args(
            &cfg.sandbox,
            &cfg.safety,
            &cfg.policy.network_allowlist,
            approval_mode,
        )
    } else {
        Vec::new()
    };
    out.extend(policy);
    out.extend(adapter.plugin_dir_args(flags, role));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fix round 7 (2026-09-20): the six read-only/UI built-ins that cannot
    /// touch the machine, the repo or production must be whole-tool allow
    /// entries so they stop prompting, while `Bash`, `Agent`/`Task`, `Skill`
    /// and `Monitor` -- which CAN reach the shell or spawn work that can --
    /// must never appear as a blanket allow.
    #[test]
    fn shipped_posture_allow_pre_approves_the_harmless_builtins_but_not_the_shell_reaching_ones() {
        let rules: Vec<&str> = SHIPPED_POSTURE_ALLOW
            .iter()
            .map(|(rule, _)| *rule)
            .collect();
        for tool in [
            "Grep",
            "Glob",
            "AskUserQuestion",
            "TodoWrite",
            "NotebookRead",
            "TaskOutput",
        ] {
            assert!(
                rules.contains(&tool),
                "expected a whole-tool allow entry for {tool}, got {rules:?}"
            );
        }
        for excluded in ["Bash", "Agent", "Task", "Skill", "Monitor", "NotebookEdit"] {
            assert!(
                !rules.contains(&excluded),
                "{excluded} must never be a blanket allow entry, got {rules:?}"
            );
        }
    }

    #[test]
    fn restrictive_codex_policy_has_one_sandbox_and_approval_option() {
        let mut cfg = CtxConfig::default();
        cfg.policy.repo_fs_write = super::super::super::policy::Stance::Deny;
        let adapter = codex::CodexAdapter::new(None)
            .with_ignore_flags_forced(true)
            .with_exec_ask_for_approval_forced(true);
        for mode in [LaunchMode::Interactive, LaunchMode::Headless] {
            let flags = policy_launch_args(
                &cfg,
                &adapter,
                &[],
                mode,
                crate::commands::ctx::prompt::PromptRole::Orchestrator,
            );
            let sandbox: Vec<_> = flags
                .windows(2)
                .filter(|w| w[0] == "--sandbox")
                .map(|w| w[1].as_str())
                .collect();
            assert_eq!(sandbox, ["read-only"], "{flags:?}");
            assert_eq!(
                flags
                    .iter()
                    .filter(|arg| *arg == "--ask-for-approval")
                    .count(),
                1,
                "{flags:?}"
            );
        }
    }

    #[test]
    fn read_only_floor_replaces_an_explicit_codex_sandbox_and_is_idempotent() {
        let adapter = codex::CodexAdapter::new(None).with_ignore_flags_forced(true);
        for sandbox in [
            vec!["--sandbox", "workspace-write"],
            vec!["-s", "workspace-write"],
            vec!["--sandbox=workspace-write"],
            vec!["-sworkspace-write"],
            vec!["--sandbox"],
            vec!["-s"],
        ] {
            let mut flags: Vec<String> = sandbox.into_iter().map(str::to_string).collect();
            extend_read_only_args(&adapter, &mut flags, LaunchMode::Headless);
            let once = flags.clone();
            extend_read_only_args(&adapter, &mut flags, LaunchMode::Headless);
            assert_eq!(flags, once);
            assert_eq!(flags.iter().filter(|arg| *arg == "--sandbox").count(), 1);
            assert!(
                flags
                    .windows(2)
                    .any(|pair| pair == ["--sandbox", "read-only"])
            );
            assert!(
                !flags.iter().any(|arg| arg.contains("workspace-write")),
                "{flags:?}"
            );
            assert_eq!(
                flags.iter().filter(|arg| *arg == "--ignore-rules").count(),
                1
            );
            assert_eq!(
                flags
                    .iter()
                    .filter(|arg| *arg == "--ignore-user-config")
                    .count(),
                1
            );
        }
    }

    /// Issue #224 review round 2: the bare "drop every guardrail" toggles
    /// pin the loosest posture just as decisively as a dedicated
    /// `--permission-mode`/`--sandbox` value, so `flags_pin_policy` must
    /// recognise them too -- see this function's own doc comment for why
    /// `safety::reserved_zirv_auto_allow_rule` depends on this.
    #[test]
    fn flags_pin_policy_recognizes_the_dangerous_guardrail_removal_toggles() {
        assert!(flags_pin_policy(&[
            "--dangerously-skip-permissions".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--dangerously-bypass-approvals-and-sandbox".to_string()
        ]));
    }

    /// Issue "codex approval hell" (2026-08-26): before this, `-c
    /// approval_policy=...`/`-c sandbox_mode=...` were invisible to
    /// `flags_pin_policy`, so zirv's own computed config-override fallback
    /// (`CodexAdapter::approval_suppression_args`) still landed after an
    /// operator's own override and won outright (codex's config resolution
    /// is last-value-wins). Both the split (`-c`/`--config` plus a following
    /// `key=value` token) and the `=`-joined single-token forms must pin.
    #[test]
    fn flags_pin_policy_recognizes_codex_config_overrides_of_approval_or_sandbox_mode() {
        assert!(flags_pin_policy(&[
            "-c".to_string(),
            "approval_policy=on-request".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "-c".to_string(),
            "sandbox_mode=read-only".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config".to_string(),
            "approval_policy=never".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config".to_string(),
            "sandbox_mode=workspace-write".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config=approval_policy=on-request".to_string()
        ]));
        assert!(flags_pin_policy(&[
            "--config=sandbox_mode=danger-full-access".to_string()
        ]));
    }

    /// The bug this round fixes: codex-cli 0.149.1 also accepts `-c`'s
    /// argument attached to the flag itself with no space (`-cKEY=VALUE`),
    /// mirroring the attached short form `classify_model_flag` already
    /// recognises for `-m` (`-mopus`). Before this, only the split
    /// (`-c`/`--config` plus a following token) and `--config=`-joined forms
    /// were recognised, so an operator spelling their override as
    /// `-capproval_policy=on-request` was invisible to `flags_pin_policy` and
    /// zirv's own computed prefix still landed after it.
    #[test]
    fn flags_pin_policy_recognizes_codexs_attached_short_config_override_form() {
        assert!(flags_pin_policy(&[
            "-capproval_policy=on-request".to_string()
        ]));
        assert!(flags_pin_policy(&["-csandbox_mode=read-only".to_string()]));
        // Precision still matters: an attached `-c` naming an unrelated key
        // must not false-positive, and `-c` itself (no attached value) is the
        // ordinary split form, already covered above.
        assert!(!flags_pin_policy(&["-cmodel=gpt-5.6-sol".to_string()]));
    }

    /// A bare `-c`/`--config` with no following token, or one overriding an
    /// unrelated key, must not false-positive: this function's whole
    /// contract is precision over coverage (see its own doc comment), and a
    /// false positive here means silently *withholding* zirv's restriction.
    #[test]
    fn flags_pin_policy_ignores_unrelated_or_dangling_config_overrides() {
        assert!(!flags_pin_policy(&[
            "-c".to_string(),
            "model=gpt-5.6-sol".to_string()
        ]));
        assert!(!flags_pin_policy(&["-c".to_string()]));
        assert!(!flags_pin_policy(&["--config".to_string()]));
        assert!(!flags_pin_policy(&[]));
    }

    /// One dimension pins the whole prefix, the same all-or-nothing
    /// granularity a bare operator `--sandbox`/`--ask-for-approval` flag
    /// already has (see the function's own doc comment): a `-c
    /// approval_policy=...` override alone, with no accompanying
    /// `sandbox_mode` override, still withholds zirv's entire computed
    /// prefix rather than just the approval half.
    #[test]
    fn flags_pin_policy_config_override_pins_the_whole_prefix_not_just_one_dimension() {
        let flags = vec!["-c".to_string(), "approval_policy=on-request".to_string()];
        assert!(flags_pin_policy(&flags));
        let cfg = CtxConfig::default();
        let codex = codex::CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_exec_ask_for_approval_forced(true);
        assert!(
            policy_launch_args(
                &cfg,
                &codex,
                &flags,
                LaunchMode::Headless,
                crate::commands::ctx::prompt::PromptRole::Orchestrator,
            )
            .is_empty(),
            "an operator's own -c approval_policy=... override must suppress zirv's entire \
             computed prefix, not just the approval flag"
        );
    }

    /// The Task 1 seam becomes load-bearing once Tasks 3 and 7 project the
    /// two postures differently. Pin that distinction at the shared seam,
    /// with codex's live capability probe forced out of the assertion.
    #[test]
    fn launch_mode_projects_the_two_postures_differently() {
        let cfg = CtxConfig::default();
        let claude = claude::ClaudeAdapter::new(None);
        let interactive = policy_launch_args(
            &cfg,
            &claude,
            &[],
            LaunchMode::Interactive,
            crate::commands::ctx::prompt::PromptRole::Orchestrator,
        );
        let headless = policy_launch_args(
            &cfg,
            &claude,
            &[],
            LaunchMode::Headless,
            crate::commands::ctx::prompt::PromptRole::Orchestrator,
        );
        assert_ne!(interactive, headless);
        // Issue #504 revision (2026-09-20): with no `chat.claude_permission_mode`
        // configured, the interactive projection omits `--permission-mode`
        // entirely so Claude Code's own configured `defaultMode` applies --
        // forcing `"default"` here silently overrode an operator's own
        // `permissions.defaultMode`, which a CLI flag outranks. Headless
        // keeps `dontAsk` hardcoded and always emitted.
        assert!(
            !interactive.iter().any(|arg| arg == "--permission-mode"),
            "an unset chat.claude_permission_mode must omit the flag: {interactive:?}"
        );
        assert!(
            headless
                .windows(2)
                .any(|w| w == ["--permission-mode", "dontAsk"])
        );

        let codex = codex::CodexAdapter::new(None)
            .with_on_request_approval_forced(true)
            .with_auto_review_forced(false)
            .with_exec_ask_for_approval_forced(true);
        let interactive = policy_launch_args(
            &cfg,
            &codex,
            &[],
            LaunchMode::Interactive,
            crate::commands::ctx::prompt::PromptRole::Orchestrator,
        );
        let headless = policy_launch_args(
            &cfg,
            &codex,
            &[],
            LaunchMode::Headless,
            crate::commands::ctx::prompt::PromptRole::Orchestrator,
        );
        assert_ne!(interactive, headless);
        assert!(
            interactive
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "on-request"])
        );
        assert!(
            headless
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"])
        );
    }

    /// The default, all-`Allow` policy must launch every adapter exactly as
    /// before this method existed: empty argv on both.
    #[test]
    fn policy_args_agree_on_no_restriction_under_the_default_policy() {
        let policy = crate::commands::ctx::policy::EffectivePolicy::default();
        assert!(
            claude::ClaudeAdapter::new(None)
                .policy_args(&policy, LaunchMode::Interactive)
                .is_empty()
        );
        assert!(
            codex::CodexAdapter::new(None)
                .policy_args(&policy, LaunchMode::Interactive)
                .is_empty()
        );
    }

    /// The same `EffectivePolicy` (`shell_exec = deny`) must carry an
    /// equivalent restriction to both adapters' real launch argv: claude's
    /// tool-deny pin, and codex's read-only sandbox paired with a suppressed
    /// approval prompt (the pairing that actually stops it escalating to a
    /// human -- see `CodexAdapter::policy_args`'s own doc comment). Neither
    /// is empty, and neither ever names the dangerous full-bypass flag.
    #[test]
    fn policy_args_carry_an_equivalent_restriction_to_both_adapters_from_the_same_policy() {
        use crate::commands::ctx::policy::{EffectivePolicy, Stance};
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };

        let claude_args =
            claude::ClaudeAdapter::new(None).policy_args(&policy, LaunchMode::Interactive);
        let codex_args =
            codex::CodexAdapter::new(None).policy_args(&policy, LaunchMode::Interactive);

        assert!(
            !claude_args.is_empty(),
            "claude must restrict: {claude_args:?}"
        );
        assert!(
            !codex_args.is_empty(),
            "codex must restrict too: {codex_args:?}"
        );
        assert!(
            claude_args.iter().any(|a| a.contains("Bash")),
            "claude denies shell execution via its tool pin: {claude_args:?}"
        );
        assert!(
            codex_args
                .windows(2)
                .any(|w| w == ["--sandbox", "read-only"]),
            "codex denies it via the read-only sandbox: {codex_args:?}"
        );
        assert!(
            codex_args
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"]),
            "and must not merely fall back to prompting for it: {codex_args:?}"
        );
        for args in [&claude_args, &codex_args] {
            assert!(
                !args.iter().any(|a| a.contains("dangerously-bypass")),
                "an equivalent-restriction mapping must never widen: {args:?}"
            );
        }
    }

    // Issue #89: the codex distiller/reviewer sandbox-asymmetry announcement.

    /// A Windows-shaped temp dir: backslashes become forward slashes, the
    /// trailing slash is dropped, and -- since the path already carries no
    /// leading slash of its own (a drive letter) -- `//` is simply
    /// prepended.
    #[test]
    fn scratchpad_rules_projects_a_windows_temp_dir() {
        let roots = scratchpad_roots_for(Path::new(r"C:\Users\x\AppData\Local\Temp\"), None, None);
        let rules = scratchpad_rules_from_roots(&roots);
        assert_eq!(
            rules,
            vec![
                "Read(//C:/Users/x/AppData/Local/Temp/claude/**)".to_string(),
                "Edit(//C:/Users/x/AppData/Local/Temp/claude/**)".to_string(),
            ]
        );
    }

    /// A Unix-shaped temp dir already carries its own leading `/`, so the
    /// convention is `//` plus the path *without* that leading slash --
    /// `//tmp/claude-501/**`, not `///tmp/claude-501/**` (three slashes).
    #[test]
    fn scratchpad_rules_projects_a_unix_temp_dir() {
        let roots = scratchpad_roots_for(Path::new("/tmp"), Some(501), None);
        let rules = scratchpad_rules_from_roots(&roots);
        #[cfg(target_os = "macos")]
        assert_eq!(
            rules,
            vec![
                "Read(//tmp/claude-501/**)".to_string(),
                "Edit(//tmp/claude-501/**)".to_string(),
                "Read(//private/tmp/claude-501/**)".to_string(),
                "Edit(//private/tmp/claude-501/**)".to_string(),
            ]
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            rules,
            vec![
                "Read(//tmp/claude-501/**)".to_string(),
                "Edit(//tmp/claude-501/**)".to_string(),
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scratchpad_rules_include_claude_code_uid_roots() {
        let roots = scratchpad_roots_for(
            Path::new("/var/folders/x/T/"),
            Some(501),
            Some(Path::new("/tmp")),
        );
        let rules = scratchpad_rules_from_roots(&roots);

        for rule in [
            "Read(//private/tmp/claude-501/**)",
            "Edit(//tmp/claude-501/**)",
        ] {
            assert!(rules.iter().any(|candidate| candidate == rule), "{rule}");
        }
    }

    /// The already-suffixed `temp_dir` is added at most once: a hook whose
    /// own `$TMPDIR` is the session scratchpad (`/tmp/claude-<uid>`) yields
    /// that root a single time, never doubled by the base default.
    #[test]
    fn scratchpad_roots_do_not_duplicate_a_uid_temp_dir() {
        let roots = scratchpad_roots_for(Path::new("/tmp/claude-501"), Some(501), None);
        #[cfg(target_os = "macos")]
        assert_eq!(
            roots,
            vec![
                "/tmp/claude-501".to_string(),
                "/private/tmp/claude-501".to_string(),
            ]
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(roots, vec!["/tmp/claude-501".to_string()]);
    }

    /// Without a `CLAUDE_CODE_TMPDIR` override the base is `/tmp` on every
    /// non-Windows host (Claude Code's macOS default and the usual Linux
    /// layout), never the hook's own `temp_dir`; macOS also carries the
    /// `/private/tmp` realpath twin so an allow rule matches both symlink
    /// sides.
    #[test]
    fn scratchpad_roots_default_to_the_tmp_base_without_an_override() {
        let roots = scratchpad_roots_for(Path::new("/var/tmp"), Some(7), None);
        #[cfg(target_os = "macos")]
        assert_eq!(
            roots,
            vec![
                "/tmp/claude-7".to_string(),
                "/private/tmp/claude-7".to_string(),
            ]
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(roots, vec!["/tmp/claude-7".to_string()]);
    }

    #[test]
    fn scratchpad_roots_prefer_the_claude_tmpdir_override() {
        assert_eq!(
            scratchpad_roots_for(
                Path::new("/var/folders/x/T"),
                Some(501),
                Some(Path::new("/scratch")),
            ),
            vec!["/scratch/claude-501".to_string()]
        );
    }

    #[test]
    fn scratchpad_roots_accept_an_already_suffixed_override() {
        for base in [
            "/tmp/claude-501",
            "/private/tmp/claude-501",
            "/scratch/claude-501/",
        ] {
            let roots = scratchpad_roots_for(Path::new(base), Some(501), Some(Path::new(base)));
            assert!(
                roots.contains(&base.trim_end_matches('/').to_string()),
                "{roots:?}"
            );
            assert!(
                roots
                    .iter()
                    .all(|root| !root.contains("claude-501/claude-501")),
                "{roots:?}"
            );
            let unique: std::collections::HashSet<_> = roots.iter().collect();
            assert_eq!(roots.len(), unique.len());
            let rules = scratchpad_rules_from_roots(&roots);
            assert!(rules.contains(&format!("Edit({}/**)", doubled_slash_rule_base(base))));
            #[cfg(target_os = "macos")]
            if base.contains("/tmp/") {
                assert!(roots.contains(&"/tmp/claude-501".to_string()), "{roots:?}");
                assert!(
                    roots.contains(&"/private/tmp/claude-501".to_string()),
                    "{roots:?}"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scratchpad_roots_add_tmp_twins_for_nested_bases_and_temp_dir() {
        for base in ["/tmp/custom", "/private/tmp/custom"] {
            let roots = scratchpad_roots_for(
                Path::new("/tmp/other/claude-501"),
                Some(501),
                Some(Path::new(base)),
            );
            for expected in [
                "/tmp/custom/claude-501",
                "/private/tmp/custom/claude-501",
                "/tmp/other/claude-501",
                "/private/tmp/other/claude-501",
            ] {
                assert!(
                    roots.iter().any(|root| root == expected),
                    "{base}: {roots:?}"
                );
            }
        }
    }

    // -- the seat role env every launch exports (issues #328/#334) ---------
}
