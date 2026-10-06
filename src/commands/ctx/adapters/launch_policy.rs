//! Sandbox/permission policy: the shipped allow/deny/ask posture, scratchpad
//! rules, and the launch argv built from an `EffectivePolicy`.
use super::*;

/// Detect explicit policy flags, including Codex config overrides and bypass flags, so operator choice wins; reject unverified short forms that could suppress zirv restrictions. (#224)
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

/// Shared default allow families make `dontAsk` usable for ordinary work; named destructive actions remain denied or ask-gated. (#104)
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
    // Allow read-only and in-conversation tools without prompting; keep mutating tools gated.
    // Monitor, Skill, and Agent/Task stay gated despite looking similar: they can run or
    // themselves invoke Bash/Write/Edit.
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
    // Allow whole toolchain families; the deny list catches named destructive subcommands. (#104)
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
    // URL fetches are ordinary work; executing a downloaded script is denied below.
    (
        "Bash(curl *)",
        "fetch a URL; piping into a shell is denied below",
    ),
    (
        "Bash(wget *)",
        "fetch a URL; piping into a shell is denied below",
    ),
    // These entries are worker capabilities, not an exhaustive command allowlist.
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

/// Project the operator scratchpad at launch because its machine-specific path cannot be a static allow rule. (#104)
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

/// Deny named destructive commands and protected file access regardless of broader allows; interpreter access makes this a tripwire, not a complete boundary. (#104, #111)
pub const SHIPPED_POSTURE_DENY: &[(&str, &str)] = &[
    (
        "Edit(~/.zirv/**)",
        "a session must never widen its own posture",
    ),
    (
        "Read(~/.claude/.credentials.json)",
        "the harness's own stored OAuth credentials",
    ),
    // Deny killing zirv before evaluating the broader ask rule: it would kill this supervisor.
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
    // Deny downloads piped into a shell using whole-command patterns.
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
    // Explicitly deny protected path reads even if the allow list later grows; mid-string globs match reordered arguments.
    (
        "Bash(cat *credentials*)",
        "reads a file conventionally named for stored credentials",
    ),
    ("Bash(cat *.aws*)", "reads AWS credential files"),
    ("Bash(cat *.ssh*)", "reads SSH private keys"),
    ("Bash(cat *.netrc*)", "reads stored HTTP credentials"),
    // Apply protected-path denies to head, tail, and diff as well as cat. (#111)
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
    // Deny alternate gh paths to destructive effects. (#111)
    (
        "Bash(gh api*DELETE*)",
        "covers both -X DELETE and --method DELETE",
    ),
    ("Bash(gh secret *)", "reads or writes repository secrets"),
    ("Bash(gh codespace ssh*)", "opens a shell into a codespace"),
];

/// Ask only for dangerous, recoverable actions; deny wins first, and unattended launches refuse asks. Ordinary and unknown commands stay prompt-free.
pub const SHIPPED_POSTURE_ASK: &[(&str, &str)] = &[
    (
        "Bash(zirv ctx supervisor override*)",
        "lifts a binding supervisor ruling; operator-only",
    ),
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
    // Inspect rebase and clean arguments before asking; routine local forms do not need a blanket prompt. (#306)
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

fn read_only_floor(adapter: &(impl AgentAdapter + ?Sized), mode: LaunchMode) -> Vec<String> {
    if mode.is_interactive() {
        adapter.interactive_read_only_args()
    } else {
        adapter.read_only_args()
    }
}

/// Registered harnesses that cannot enforce read-only for `mode`; auto-routing excludes them for read-only work.
pub fn floorless_adapter_names(mode: LaunchMode) -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .filter(|(_, ctor)| !ctor(None).read_only_floor_available(mode.is_interactive()))
        .map(|(name, _)| *name)
        .collect()
}

/// Fail closed: a worker labelled read-only must not launch on a harness with no enforced read-only floor.
pub fn require_read_only_floor(
    adapter: &(impl AgentAdapter + ?Sized),
    mode: LaunchMode,
) -> Result<(), String> {
    if !read_only_floor(adapter, mode).is_empty() {
        return Ok(());
    }
    let floorless = floorless_adapter_names(mode);
    let capable: Vec<&str> = ADAPTERS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !floorless.contains(name))
        .collect();
    Err(format!(
        "harness '{}' cannot enforce read-only here, so a read-only worker would be able to write files and run commands; refusing to launch. Harnesses that can enforce read-only: {}",
        adapter.name(),
        capable.join(", ")
    ))
}

/// Resolve a registered adapter's structural read-only flags without needing it installed; unknown names remain `None` so callers can refuse.
pub fn read_only_args_for_agent_name(name: &str, mode: LaunchMode) -> Option<Vec<String>> {
    ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .map(|(_, ctor)| {
            let adapter = ctor(None);
            // Resolve the reviewer's structural read-only pin before launch; absent config uses the normal static default. (#89)
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
    let mut floor = read_only_floor(adapter, mode);
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

/// Append the adapter's writable roots (git dirs and zirv state subdirectories)
/// to a non-empty policy baseline, so a supervised seat's own `zirv` calls do
/// not escalate (#845). Empty `extra` (operator-pinned policy or sandbox off)
/// stays empty: the operator's posture is never widened.
pub fn with_workload_writable_roots(
    mut extra: Vec<String>,
    adapter: &(impl AgentAdapter + ?Sized),
    cwd: &Path,
    state: &super::super::state::StateDir,
) -> Vec<String> {
    if !extra.is_empty() {
        extra.extend(adapter.extra_writable_root_args(cwd, state));
    }
    extra
}

/// Separate approval posture from the actual CLI surface for dashboard panes: an unattended interactive pane must use interactive-safe read-only flags. (#326)
/// `approval_mode` feeds `default_sandbox_args`, safe under either value; `surface_mode`
/// feeds `policy_args`, where a codex Deny-stance projection is genuinely surface-unsafe.
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
        adapter.default_sandbox_args_for_role(
            &cfg.sandbox,
            &cfg.safety,
            &cfg.policy.network_allowlist,
            approval_mode,
            Some(role),
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

    #[test]
    fn workload_roots_follow_a_non_empty_baseline_and_never_widen_an_empty_one() {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_root = tempfile::tempdir().expect("tempdir");
        let state =
            crate::commands::ctx::state::StateDir::from_path(state_root.path().to_path_buf());
        let codex = super::super::codex::CodexAdapter::new(None);
        let empty = with_workload_writable_roots(Vec::new(), &codex, repo.path(), &state);
        assert!(
            empty.is_empty(),
            "operator-pinned launch must stay empty: {empty:?}"
        );
        let out = with_workload_writable_roots(
            vec!["--sandbox".to_string(), "workspace-write".to_string()],
            &codex,
            repo.path(),
            &state,
        );
        assert_eq!(&out[..2], ["--sandbox", "workspace-write"]);
        assert!(
            out.iter()
                .any(|a| a.starts_with("sandbox_workspace_write.writable_roots=[")),
            "{out:?}"
        );
    }

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

    #[test]
    fn a_read_only_launch_is_refused_on_an_empty_floor_harness_naming_the_capable_ones() {
        let cursor = cursor::CursorAdapter::new(None);
        for mode in [LaunchMode::Headless, LaunchMode::Interactive] {
            let message = require_read_only_floor(&cursor, mode).expect_err("empty floor");
            assert!(message.contains("'cursor-agent'"), "{message}");
            for capable in ["claude", "codex", "copilot"] {
                assert!(message.contains(capable), "{message}");
            }
            let capable_list = message.rsplit(": ").next().unwrap_or_default();
            for floorless in ["cursor-agent", "goose", "grok", "muse"] {
                assert!(!capable_list.contains(floorless), "{message}");
            }
        }
    }

    /// The refusal lists capable harnesses without launching any: building it must write no
    /// policy or config file (gemini's lands in the temp dir when no state dir resolves).
    #[test]
    fn building_the_read_only_refusal_writes_no_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("state");
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            ("TMPDIR", Some(tmp.path().to_str().expect("utf8"))),
            (
                crate::commands::ctx::state::STATE_ENV,
                Some(state.path().to_str().expect("utf8")),
            ),
        ]);
        let cursor = cursor::CursorAdapter::new(None);
        for mode in [LaunchMode::Headless, LaunchMode::Interactive] {
            assert!(require_read_only_floor(&cursor, mode).is_err());
        }
        for dir in [tmp.path(), state.path()] {
            let written: Vec<_> = std::fs::read_dir(dir).expect("read_dir").collect();
            assert!(written.is_empty(), "{written:?}");
        }
    }

    /// Availability is a cheap pre-check; the launch re-validates the final adapter's real args,
    /// so a harness that claims a floor but cannot materialize it is still refused.
    #[test]
    fn a_floor_that_cannot_be_materialized_at_launch_is_refused() {
        let blocker = tempfile::NamedTempFile::new().expect("file");
        let adapter =
            opencode::OpenCodeAdapter::new(None).with_state_root(blocker.path().join("state"));
        assert!(adapter.read_only_floor_available(false));
        for mode in [LaunchMode::Headless, LaunchMode::Interactive] {
            let message = require_read_only_floor(&adapter, mode).expect_err("empty floor");
            assert!(message.contains("cannot enforce read-only"), "{message}");
        }
    }

    #[test]
    fn a_read_only_launch_is_allowed_where_the_floor_is_enforced() {
        for name in ["claude", "codex", "copilot"] {
            let (_, ctor) = ADAPTERS.iter().find(|(n, _)| *n == name).expect("adapter");
            for mode in [LaunchMode::Headless, LaunchMode::Interactive] {
                assert_eq!(require_read_only_floor(ctor(None).as_ref(), mode), Ok(()));
            }
        }
    }

    #[test]
    fn floorless_adapter_names_lists_exactly_the_empty_floor_harnesses() {
        let names = floorless_adapter_names(LaunchMode::Headless);
        for floorless in ["cursor-agent", "goose", "grok", "muse"] {
            assert!(names.contains(&floorless), "{names:?}");
        }
        for capable in ["claude", "codex", "copilot"] {
            assert!(!names.contains(&capable), "{names:?}");
        }
    }
}
