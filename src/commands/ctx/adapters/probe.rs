//! Program resolution, cmd-shim/PowerShell launcher handling, installed-binary
//! liveness probing, its on-disk cache, and transcript/session file lookup.
use super::*;

/// The program invocation at the head of an argv: the binary plus the leading
/// arguments before the first flag, which is what `sh wrapper.sh --foo` and
/// `/usr/bin/env claude -p x` both need. Anything past that is the operator's
/// own flags and has no business being passed to a `--help` probe.
pub fn program_invocation(launch: &[String]) -> Option<(String, Vec<String>)> {
    let (program, rest) = launch.split_first()?;
    let args = rest
        .iter()
        .take_while(|arg| !arg.starts_with('-'))
        .cloned()
        .collect();
    Some((program.clone(), args))
}

/// A program invocation rewritten so the host OS can actually execute it.
/// `prefix` is the tokens that have to lead the original arguments, empty
/// whenever the program can be spawned directly (always, off Windows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProgram {
    pub program: String,
    pub prefix: Vec<String>,
}

impl ResolvedProgram {
    /// The invocation exactly as written: no launcher, nothing prepended.
    pub fn direct(program: &str) -> Self {
        Self {
            program: program.to_string(),
            prefix: Vec::new(),
        }
    }
}

/// Resolves `program` the way the OS itself would, and rewrites the
/// invocation when what it resolves to cannot be handed to the process
/// creation call directly.
///
/// Off Windows this is the identity: `execvp` honors the shebang of anything
/// on `PATH`, so there is nothing to rewrite.
///
/// On Windows it matters. An npm-installed `claude` is `claude.cmd`, and the
/// two resolvers zirv uses disagreed about it: `std::process::Command` only
/// ever appends `.exe`, while portable-pty's `search_path` honors `PATHEXT`,
/// finds `claude.cmd`, and then hands it to `CreateProcessW` as
/// `lpApplicationName`, which rejects it with `ERROR_BAD_EXE_FORMAT` (193).
/// Resolving `PATH` plus `PATHEXT` here and routing a `.cmd`/`.bat` through
/// `cmd.exe` (a `.ps1` through PowerShell) is what makes the most common
/// Windows install layout launch at all.
///
/// A program that resolves to nothing is returned untouched, so a missing
/// binary still fails with the OS's own "not found" rather than a zirv error
/// about a path that does not exist. `Err` is reserved for the one case zirv
/// can name before spawning and knows will fail: a bare name that `PATHEXT`
/// resolved to a file type with no launcher. A program written with a
/// directory in it is never an error here, whatever it ends in: the caller
/// named that exact file, and a wrapper this code has never heard of is
/// theirs to be told about by the OS, exactly as before.
#[cfg(windows)]
pub fn resolve_program(program: &str) -> Result<ResolvedProgram, String> {
    let Some((resolved, from_path)) = resolve_on_path(program) else {
        return Ok(ResolvedProgram::direct(program));
    };
    let extension = resolved
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let found = resolved.display().to_string();
    match extension.as_str() {
        "cmd" | "bat" => Ok(ResolvedProgram {
            program: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            prefix: vec!["/c".to_string(), found],
        }),
        "ps1" => Ok(ResolvedProgram {
            program: "powershell".to_string(),
            prefix: vec!["-NoProfile".to_string(), "-File".to_string(), found],
        }),
        other if from_path && !matches!(other, "exe" | "com" | "") => Err(format!(
            "cannot launch '{program}': it resolves to '{found}', which Windows cannot execute \
             directly (CreateProcess accepts only .exe and .com). zirv runs .cmd and .bat through \
             cmd.exe and .ps1 through PowerShell, but it has no launcher for '.{other}'."
        )),
        // Directly executable, or named explicitly enough that the caller
        // owns the outcome. Deliberately keeps the program spelled the way it
        // was written rather than substituting the resolved path: nothing
        // about the launch changes, so nothing about it should.
        _ => Ok(ResolvedProgram::direct(program)),
    }
}

#[cfg(not(windows))]
pub fn resolve_program(program: &str) -> Result<ResolvedProgram, String> {
    Ok(ResolvedProgram::direct(program))
}

/// I: the flags an adapter-built command carries, with any launcher prefix
/// dropped. On a Windows machine where the adapter's own program resolves to
/// a real npm `.cmd` shim, every command an adapter builds starts `cmd.exe /c
/// <shim>`, and those tokens are not what a caller asserting on agent flags
/// means to inspect. `program` is a plain `&str` (rather than `&dyn
/// AgentAdapter`) since nothing else about the adapter is needed -- callers
/// get it from the adapter's own public `AgentAdapter::program()`. Was
/// duplicated byte-for-byte in `claude.rs` and `codex.rs`'s own test modules
/// before this; both now call this one copy. No longer test-only (issue
/// I-2): `checks::argv::run_codex_exec` (`zirv verify`'s own
/// `ZCHK-ARGV-CODEX-EXEC`) needs the identical launcher-stripping to avoid
/// failing on the operator's own Windows `.cmd`-shim install.
pub(crate) fn built_args(program: &str, cmd: &std::process::Command) -> Vec<String> {
    let launcher = resolve_program(program)
        .map(|resolved| resolved.prefix.len())
        .unwrap_or(0);
    cmd.get_args()
        .skip(launcher)
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

/// The cmd.exe metacharacters that, appearing RAW in an argument, cmd.exe
/// re-parses out of its own `/c` command line rather than passing through to
/// the shim it invokes. portable-pty and `std::process` both append a
/// no-whitespace metachar-bearing argument to a Windows command line unquoted,
/// and an embedded `"` toggles cmd.exe out of any quoting that *was* added
/// (BatBadBut / CVE-2024-24576's quote-toggle). Newline and carriage return
/// terminate the command line outright. Any of these in a shim-form argument
/// is therefore a command-injection primitive, not a literal argument value.
///
/// Review finding (#395): `pub(crate)` and not `#[cfg(windows)]`-gated (the
/// guard's own use of it below still is) -- `config::validate_endpoint_target`
/// also scans an `[endpoint.*]` `base_url` against this exact list at load
/// time, on every platform a config can be validated on, so a base_url that
/// would be refused by [`guard_cmd_shim_reparse`] on a Windows launch is
/// rejected up front rather than reaching codex's `-c` argv as a token this
/// guard then has to fail closed on.
pub(crate) const CMD_REPARSE_METACHARS: &[char] =
    &['&', '|', '<', '>', '^', '(', ')', '%', '!', '"', '\n', '\r'];

/// Whether `program` + `args` is the `cmd.exe /c <shim>` launcher form that
/// [`resolve_program`] produces for a `.cmd`/`.bat` on Windows: the program's
/// file stem is `cmd` and the first argument is `/c`. Matched structurally
/// (case-insensitively) rather than by identity with a specific `COMSPEC`
/// value, so a full-path or upper-cased `CMD.EXE` is recognised too.
#[cfg(windows)]
fn is_cmd_shim_launch(program: &str, args: &[String]) -> bool {
    let program_is_cmd = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.eq_ignore_ascii_case("cmd"))
        .unwrap_or(false);
    program_is_cmd
        && args
            .first()
            .map(|first| first.eq_ignore_ascii_case("/c"))
            .unwrap_or(false)
}

/// FIX D (defense-in-depth): the number of leading `args` tokens that are the
/// zirv-controlled launcher prefix, when `program` + `args` is a Windows
/// launcher form whose command line is reparsed before it reaches the real
/// script -- either the `cmd.exe /c <shim>` form (a `.cmd`/`.bat`) or the
/// `powershell -NoProfile -File <script>` form (a `.ps1`), both of which
/// [`resolve_program`] produces. `None` when it is neither, so a direct
/// `.exe` or an `sh <script>` fake agent is not keyed on at all. Keyed on the
/// `/c` / `-File` structure rather than only the launcher basename, so the
/// guard covers whichever launcher the resolver actually inserted.
#[cfg(windows)]
fn reparse_launcher_prefix(program: &str, args: &[String]) -> Option<usize> {
    let stem = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())?
        .to_ascii_lowercase();
    match stem.as_str() {
        "cmd" => args
            .first()
            .map(|first| first.eq_ignore_ascii_case("/c"))
            .unwrap_or(false)
            // `/c` and the shim path are both zirv-controlled.
            .then_some(2),
        "powershell" | "pwsh" => {
            // Everything through the `-File <script>` pair is the launcher
            // prefix; the script's own arguments follow it.
            let file_at = args
                .iter()
                .position(|arg| arg.eq_ignore_ascii_case("-File"))?;
            (file_at + 1 < args.len()).then_some(file_at + 2)
        }
        _ => None,
    }
}

/// FIX (command-injection defense): fail-closed guard for the one launch shape
/// where a downstream argv element becomes cmd.exe *source text* rather than a
/// literal argument. When [`resolve_program`] rewrites an npm-installed
/// `claude.cmd` to `cmd.exe /c <shim>`, cmd.exe parses the whole appended
/// command line before invoking the shim, so any argument after the shim path
/// that carries a cmd.exe metacharacter is re-interpreted as a command. Repo-
/// controlled strings (an injected system prompt, a passed-through flag) reach
/// this argv, so an unguarded metacharacter there is arbitrary code execution
/// on a victim who merely runs a supervised session in a hostile checkout.
///
/// This rejects such a launch outright rather than trying to quote around
/// cmd.exe (which the embedded-quote toggle defeats). It is deliberately a
/// pure decision function over the already-resolved `program`/`args`, called
/// at every spawn seam (`supervise::spawn_tapped` for the headless
/// `exec`/`loop` path; the `CommandBuilder` assembly in `wrap` and
/// `dash::pane` for the pty path), so there is one metacharacter policy.
///
/// A no-op off Windows, and on Windows for any launch that is not the shim
/// form: a direct `.exe`, an `sh <script>` fake agent, or a program with no
/// launcher prefix is spawned exactly as before. zirv's own flags never carry
/// these characters, so only injected content is ever rejected. The two shim-
/// prefix tokens themselves (`/c` and the shim path) are zirv-controlled and
/// skipped.
pub fn guard_cmd_shim_reparse(program: &str, args: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    {
        if let Some(prefix) = reparse_launcher_prefix(program, args) {
            for arg in args.iter().skip(prefix) {
                if let Some(bad) = arg.chars().find(|c| CMD_REPARSE_METACHARS.contains(c)) {
                    return Err(format!(
                        "refusing to launch: argument '{arg}' contains the cmd.exe \
                         metacharacter {bad:?}. zirv routes this agent through a Windows \
                         launcher ('cmd.exe /c' for an npm-installed '.cmd' shim, or \
                         'powershell -File' for a '.ps1'), which would re-parse that character \
                         as a command rather than pass it through. This is a fail-closed \
                         backstop against command injection; zirv's own arguments never contain \
                         these characters, and untrusted content (the composed system prompt, a \
                         headless task prompt) is kept off this argv entirely."
                    ));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args);
    }
    Ok(())
}

/// Whether spawning `program` resolves to the Windows `cmd.exe /c <shim>`
/// launcher form (an npm-installed `.cmd`), where cmd.exe reparses the whole
/// downstream command line. The adapters use it to move a headless prompt --
/// and any folded mail -- onto the child's stdin on exactly the launch shape
/// where an argv token would otherwise be reparsed. Always `false` off
/// Windows, and for a directly executable program.
pub fn launches_through_cmd_shim(program: &str) -> bool {
    #[cfg(windows)]
    {
        match resolve_program(program) {
            Ok(resolved) => is_cmd_shim_launch(&resolved.program, &resolved.prefix),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        false
    }
}

/// Whether spawning the argv `launch` puts its downstream tokens through a
/// Windows launcher that reparses them -- either the `cmd.exe /c <shim>` form
/// (an npm-installed `.cmd`) or the `powershell -File <script>` form (a
/// `.ps1`). Unlike [`launches_through_cmd_shim`], which is given only a bare
/// program name to re-resolve, this handles an argv that is **already
/// resolved** to a launcher: `chat::build_launch`/`ClaudeAdapter::base` hand
/// `wrap`/`dash_orchestrator_pane` an argv whose head is literally `cmd.exe`
/// (or `powershell`), so re-resolving that head finds a plain `.exe` and would
/// wrongly report "not a shim", leaving the forced-file-form defence inert on
/// the interactive path. Recognising the resolved launcher structure directly
/// (via [`reparse_launcher_prefix`]) is what keeps that defence engaged.
///
/// Falls back to resolving the head program for an argv that has *not* been
/// resolved yet (a raw `wrap` command such as `["claude", "--resume"]`), so
/// both call shapes reach the same verdict. Always `false` off Windows.
pub fn launch_reparses_through_shim(launch: &[String]) -> bool {
    #[cfg(windows)]
    {
        let Some((program, rest)) = launch.split_first() else {
            return false;
        };
        // An already-resolved `cmd.exe /c <shim>` or `powershell -File <script>`
        // argv: the launcher reparses everything past its own prefix.
        if reparse_launcher_prefix(program, rest).is_some() {
            return true;
        }
        // Otherwise the head is an ordinary program name that `resolve_program`
        // may still route through a launcher.
        launches_through_cmd_shim(program)
    }
    #[cfg(not(windows))]
    {
        let _ = launch;
        false
    }
}

/// `std::process::Command` -> the flat `program, arg, arg, ...` form
/// [`launch_reparses_through_shim`] wants. Shared here rather than
/// duplicated per call site (`exec.rs`, `run_loop.rs`; `dash/mod.rs` keeps
/// its own private copy, established first and not worth churning): a probe
/// command built purely to answer "what launcher shape would this be" (no
/// real prompt text on it yet) is flattened the same way regardless of
/// which module is asking.
pub fn flatten_command(command: std::process::Command) -> Vec<String> {
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    argv
}

/// `PATH` plus `PATHEXT`, the search the Windows shell performs and
/// `std::process::Command` does not. A program that already carries a
/// directory is looked for where it says, not on `PATH`; the flag reports
/// which of the two happened, because only a `PATH` hit is a name the shell
/// itself would have claimed to be executable.
#[cfg(windows)]
fn resolve_on_path(program: &str) -> Option<(PathBuf, bool)> {
    if program.is_empty() {
        return None;
    }
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
        .collect();

    let named_directory = program.contains('/') || program.contains('\\');
    let bases: Vec<PathBuf> = if named_directory {
        vec![PathBuf::from(program)]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(program))
                    .collect()
            })
            .unwrap_or_default()
    };

    let from_path = !named_directory;
    for base in bases {
        // An explicit extension that exists wins outright, so
        // `claude.cmd` is never resolved to `claude.cmd.exe`.
        if base.extension().is_some() && base.is_file() {
            return Some((base, from_path));
        }
        for extension in &extensions {
            let candidate = PathBuf::from(format!("{}{extension}", base.display()));
            if candidate.is_file() {
                return Some((candidate, from_path));
            }
        }
        if base.is_file() {
            return Some((base, from_path));
        }
    }
    None
}

/// Whether `program`'s binary genuinely exists on disk -- either at an
/// explicit path, or somewhere on `PATH` (`PATHEXT`-aware on Windows).
///
/// This is deliberately a *stronger* claim than `resolve_program`/`ready()`
/// make: `resolve_program` is fail-open by design for a name it cannot find
/// (a program that resolves to nothing is spawned exactly as written, so a
/// genuinely missing binary fails with the OS's own "not found" rather than a
/// zirv-invented error), and several call sites rely on exactly that
/// fail-open behavior (`agent_bin` naming a not-yet-real path still has to
/// fall through to whichever adapter it actually matches by name, not error
/// out early). `harness_prompt_lines` is the one caller that turns "ready"
/// into a concrete invitation (`zirv agent <name> "<prompt>"`) an orchestrator
/// may act on immediately, so it alone needs this stronger check layered on
/// top of -- never in place of -- `ready()`.
pub fn program_is_present(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        resolve_on_path(program).is_some()
    }
    #[cfg(not(windows))]
    {
        if program.contains('/') {
            return Path::new(program).is_file();
        }
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
            .unwrap_or(false)
    }
}

/// Issue #298 ("capability-gated injection"): a cheap, filesystem/config-only
/// verdict on whether a capability can actually be exercised right now -- no
/// process spawn, no network call, no more than a few `stat`s. Only `Live`
/// earns a line in the injected harness roster: `Absent` costs the session
/// zero prompt bytes rather than annotating a capability it cannot use, and
/// `Unknown` still renders the line, because a probe that could not decide
/// must never cost a session a capability it might actually have -- the same
/// fail-open discipline [`program_is_present`]'s own doc comment already
/// holds `resolve_program`/`ready()` to.
///
/// Never assert absence in injected text: an omitted line is honest, but a
/// rendered "not installed" claim can be wrong (see [`program_is_present`]'s
/// own doc comment -- codex's real install root on this repo's own dev
/// machine is one a plain `PATH` walk never reaches). `Absent`'s `String` is
/// a diagnostic reason for an operator-facing surface (`compile --measure`'s
/// own note column) only; nothing in this module ever prints it into a
/// session's own prompt.
///
/// `pub(crate)`: also the verdict [`adapter_liveness`] hands back to
/// `chat::harness_list`, so the chat banner's roster and this module's own
/// injected roster (`harness_roster_lines`) read the same fail-open rule off
/// one type instead of each defining its own notion of "live".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Liveness {
    Live,
    Absent(String),
    Unknown(String),
}

impl Liveness {
    /// `Live` and `Unknown` both earn a line (fail-open); only a confirmed
    /// `Absent` is omitted. See this type's own doc comment.
    pub(crate) fn emits_line(&self) -> bool {
        !matches!(self, Liveness::Absent(_))
    }
}

/// `~/rest` -> the operator's real home directory joined with `rest`, using
/// [`crate::utils::home_dir`] (`HOME`/`USERPROFILE`) -- deliberately the
/// *operator's* environment, never anything a repo checkout can name (see
/// [`known_install_roots`]'s own doc comment). Any other shape (no leading
/// `~/`, or a home dir that cannot be resolved) is returned unchanged.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => crate::utils::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|_| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

/// Extra install roots the issue #298 liveness probe consults for
/// `adapter_name` after `PATH` and any `agent_bin` override come up empty --
/// codex-cli on this repo's own dev machine lives at a real, working install
/// outside `PATH` (see CLAUDE.md's "This Windows dev machine" section),
/// which [`program_is_present`]'s plain `PATH` walk alone reports as absent.
///
/// A fixed Rust table, never a config read: repo-owned surfaces may only
/// *narrow* what a probe can conclude (see `context.rs`'s own trust model),
/// and an operator who wants a *different* install location already has
/// `agent_bin` (itself `REPO_FORBIDDEN`) for that -- widening the search is
/// the one thing a repo checkout must never be able to do on its own, so
/// this function takes no `CtxConfig`/repo path at all, which is what keeps
/// the acceptance criterion "a repo-layer config change cannot flip an
/// adapter from `Absent` to `Live`" true by construction.
fn known_install_roots(adapter_name: &str) -> &'static [&'static str] {
    match adapter_name {
        "codex" => &["~/AppData/Local/Programs/OpenAI/Codex/bin"],
        _ => &[],
    }
}

/// Every directory the widened liveness probe checks for `adapter_name`'s
/// `program`: `PATH`, then [`known_install_roots`]. Empty when `program`
/// already names a directory (an absolute/relative path, or an `agent_bin`
/// override) -- matching [`program_is_present`]'s own convention that such a
/// program is checked only at that one exact path.
fn liveness_search_dirs(adapter_name: &str, program: &str) -> Vec<PathBuf> {
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    dirs.extend(
        known_install_roots(adapter_name)
            .iter()
            .map(|root| expand_home(root)),
    );
    dirs
}

/// [`program_is_present`], widened with [`known_install_roots`]: reuses
/// `program_is_present`'s own PATHEXT-aware Windows resolution and plain
/// Unix file check unchanged -- handing it a full candidate path exercises
/// exactly the same code path an absolute `agent_bin` override already does
/// -- so widening only ever adds candidate *directories*; it never changes
/// how a program name or an explicit override is resolved.
fn program_is_present_widened(adapter_name: &str, program: &str) -> bool {
    if program_is_present(program) {
        return true;
    }
    liveness_search_dirs(adapter_name, program)
        .iter()
        .any(|dir| program_is_present(dir.join(program).to_string_lossy().as_ref()))
}

/// Whether any directory [`liveness_search_dirs`] walked for `program` could
/// not be checked for a reason other than a clean "not found" -- permission
/// denied, a `PATH` entry that turns out to be a plain file rather than a
/// directory, or similar. Only ever consulted after
/// [`program_is_present_widened`] has already come up empty: a probe this
/// inconclusive must never be reported as a confirmed absence. Approximate
/// by design (it stats the bare program name, not every `PATHEXT`
/// candidate) -- this is a fail-open safety net, not the presence check
/// itself.
fn inconclusive_reason(adapter_name: &str, program: &str) -> Option<String> {
    liveness_search_dirs(adapter_name, program)
        .into_iter()
        .find_map(|dir| {
            let candidate = dir.join(program);
            match std::fs::metadata(&candidate) {
                Ok(_) => None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => Some(format!("could not check '{}': {e}", candidate.display())),
            }
        })
}

/// Issue #690: the machine a selection test assumes, stated rather than
/// inherited -- `only_installed` names every adapter whose program is
/// `Live`, and every other adapter is confidently `Absent`. Without an
/// injected oracle a test of the fallback reads the developer's own `PATH`,
/// passing on a laptop with claude and codex installed and failing on a CI
/// runner with neither.
///
/// `pub(crate)`, and deliberately not inside this module's own `mod tests`:
/// the seam belongs to every module whose tests reach selection at all --
/// `status.rs`'s `chat:` line, `chat.rs`'s harness rule, `usage.rs`'s
/// provider readout -- and each of them re-rolling its own closure is how
/// two would quietly end up asserting different machines.
#[cfg(test)]
pub(crate) fn only_installed(names: &'static [&'static str]) -> impl Fn(&str, &str) -> Liveness {
    move |name: &str, program: &str| {
        if names.contains(&name) {
            Liveness::Live
        } else {
            Liveness::Absent(format!("no '{program}' found"))
        }
    }
}

/// Every adapter installed: the machine every pre-#690 selection test
/// silently assumed, now said out loud.
#[cfg(test)]
pub(crate) fn everything_installed() -> impl Fn(&str, &str) -> Liveness {
    |_name: &str, _program: &str| Liveness::Live
}

/// A probe that reaches no verdict at all -- an unreadable `PATH` entry, a
/// permission error. Fail-open: `Unknown` must leave selection behaving
/// exactly as it did before presence was ever consulted.
#[cfg(test)]
pub(crate) fn nothing_decidable() -> impl Fn(&str, &str) -> Liveness {
    |_name: &str, program: &str| Liveness::Unknown(format!("could not check '{program}'"))
}

/// The full issue #298 probe: `Live` when [`program_is_present_widened`]
/// finds `program`, `Unknown` when the search could not reach a confident
/// verdict (fail-open -- see [`Liveness`]'s own doc comment), `Absent`
/// otherwise.
///
/// `pub(crate)` because it is also the presence oracle every *production*
/// caller hands to the injected selection path ([`resolve_default_with_
/// presence`] and its callers in `status.rs`/`chat.rs`): one probe, named
/// once, so no surface can drift into a second notion of "installed".
pub(crate) fn liveness_probe(adapter_name: &str, program: &str) -> Liveness {
    if program.is_empty() {
        return Liveness::Absent("no program name".to_string());
    }
    if program_is_present_widened(adapter_name, program) {
        return Liveness::Live;
    }
    match inconclusive_reason(adapter_name, program) {
        Some(reason) => Liveness::Unknown(reason),
        None => Liveness::Absent(format!("no '{program}' found")),
    }
}

/// The one sentence zirv uses for "this adapter's program is not on this
/// machine", wherever that answer is reached from.
///
/// Issue #690 (remaining scope) added a second way to reach it -- the launch
/// pre-flight ([`refuse_if_program_absent_with_presence`]) that decides it
/// *before* the spawn rather than from the spawn's own `NotFound` -- and a
/// second `format!` would have been a second wording to keep in step. Factored
/// here instead, so the fast answer and the slow one are byte-identical: the
/// pre-flight only ever turns a slow failure into a fast one, and an operator
/// comparing the two never has to wonder whether they mean different things.
///
/// All three ways out, in the same words [`resolve_default_with_presence`]'s
/// aggregate "no harness is installed" error already uses. The `agent_bin`
/// remedy is the one that matters most here and used to be missing: this
/// message is reached precisely when no `agent_bin` is set (the pre-flight
/// does not probe an override at all), so its reader may well be someone
/// whose harness *is* installed, just not anywhere a `PATH` walk reaches --
/// see [`known_install_roots`] and CLAUDE.md's own note that codex on this
/// repo's dev machine lives at a real install outside `PATH`. Telling that
/// operator to install software they already have is the wrong answer, and
/// it was the only one this sentence gave.
fn program_not_found_message(adapter_name: &str, program: &str) -> String {
    format!(
        "adapter '{adapter_name}': program '{program}' not found. Install it so its program is \
         on PATH, or point `agent_bin` at it in ~/.zirv/ctx.toml, or name an installed one with \
         --agent."
    )
}

/// Formats a launch error with context about which harness and program failed.
/// When the error is NotFound, includes a suggestion to install the harness or use --agent.
pub(crate) fn format_launch_error(
    error: &(dyn std::error::Error + 'static),
    adapter_name: &str,
    program: &str,
) -> String {
    // Try to get the io::Error kind directly if available
    if let Some(io_err) = error.downcast_ref::<std::io::Error>()
        && io_err.kind() == std::io::ErrorKind::NotFound
    {
        return program_not_found_message(adapter_name, program);
    }

    // For any other error, include adapter/program context
    format!(
        "adapter '{}': program '{}' failed to start: {}",
        adapter_name, program, error
    )
}

/// Issue #690 (remaining scope): the launch pre-flight. Once a launch path
/// has resolved the adapter it is about to *start*, and before it engages
/// pacing, usage polling or the macOS Keychain-reading path any of that
/// drags in, refuse outright if that adapter's program is confidently not on
/// this machine.
///
/// The defect this exists for: on a machine with no harness installed,
/// `zirv ctx agent claude "say hi"` used to warn about Keychain access for a
/// harness the operator does not have, sit out `[pace] blind_delay_secs` of
/// safety delay because that harness has no usage source, and only then
/// report that `claude` is not a program. None of that machinery has anything
/// to pace or poll when there is no process to launch.
///
/// Three rules, none of them new -- each is the rule an existing seam in this
/// module already holds to:
///
/// 1. Fail-open. Only [`Liveness::Absent`] refuses; `Live` and `Unknown` both
///    proceed exactly as before the pre-flight existed. A probe that could
///    not decide must never cost a launch that would have worked -- see
///    [`Liveness`]'s and [`program_is_present`]'s own doc comments for how
///    wrong this probe is allowed to be.
/// 2. Never substitute. This only ever turns a slow failure into a fast one;
///    it chooses nothing. An explicitly named `--agent`, or a configured
///    `agent`, fails here under *its own* name -- the invariant
///    [`resolve_default_with_presence`]'s G/G3 notes pin, that a harness the
///    operator named is never silently swapped for another, is not weakened
///    by making its failure arrive sooner.
/// 3. No probe at all while `agent_bin` is set. An operator-set override
///    need not be a path a `stat` can answer (the `sh <wrapper>.sh` shape
///    this codebase's own fixtures use throughout resolves to nothing on
///    disk), so probing it would hard-fail working setups. This is
///    [`resolve_default_with_presence`]'s own `consult_presence = bin.
///    is_none()` rule, reused rather than a second rule invented beside it.
///
/// It is the caller's job to apply this only where the program about to be
/// spawned actually *is* `adapter.program()`. `exec`'s explicit
/// `-- <command>` passthrough and `wrap`'s wrapped argv are the operator's
/// own program, not this adapter's, and refusing those would worsen a
/// session rather than fail one faster.
///
/// `_with_presence` and no un-injected twin, unlike [`resolve_default`]/
/// [`resolve_default_with_presence`]: every production caller is a *launch
/// entry point* (`exec::run_with_clock`, `run_loop::run_with_clock`) that
/// already names [`liveness_probe`] once for its whole call, so a second
/// wrapper naming it again here would only be a second place for a caller
/// to reach the probe from -- and dead code besides.
pub(crate) fn refuse_if_program_absent_with_presence(
    adapter: &dyn AgentAdapter,
    cfg: &CtxConfig,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> CtxResult<()> {
    // Rule 3, before anything touches the filesystem.
    if cfg.agent_bin.is_some() {
        return Ok(());
    }
    match present(adapter.name(), adapter.program()) {
        // Rule 1: only a confident absence refuses.
        Liveness::Absent(_) => {
            Err(program_not_found_message(adapter.name(), adapter.program()).into())
        }
        Liveness::Live | Liveness::Unknown(_) => Ok(()),
    }
}

/// One cached liveness verdict, keyed by [`ProbeCache::key`] (adapter name,
/// program, resolved `agent_bin` override). `checked_at` is a plain
/// `now_secs()`-style timestamp the caller supplies -- this module reads no
/// clock itself, the same discipline `compile.rs`/`memory::render_for_prompt`
/// already hold to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedProbe {
    checked_at: u64,
    live: bool,
    reason: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProbeCacheFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: std::collections::BTreeMap<String, CachedProbe>,
}

const PROBE_CACHE_VERSION: u32 = 1;

/// How long a cached `Live`/`Absent` verdict is trusted before [`ProbeCache`]
/// re-probes. Issue #298 pairs this cache with issue 20 in the same batch (a
/// byte-stable injected prefix across a session's turns, which upstream
/// prompt caching depends on): a strictly per-`SessionId` cache would give
/// zero reuse to the one launch path that most needs it (`run_loop` mints a
/// fresh `SessionId` every cycle -- see `StateDir::adoption`'s own doc
/// comment), so this cache is scoped per repository instead (see
/// `StateDir::probes`) and self-corrects on this TTL rather than never at
/// all.
const PROBE_CACHE_TTL_SECS: u64 = 3600;

/// A per-repository cache of [`Liveness`] probe verdicts, backed by one JSON
/// file under the state dir (`StateDir::probes`). Missing or unparseable
/// (including a version mismatch) reads back empty, never an error: a cache
/// miss just means the next probe is a real one, exactly as if the cache did
/// not exist. A failed *write* is silently ignored for the same reason
/// `score.rs::save_checkpoint` already is -- a cache that cannot be written
/// costs the next call a real probe, which is exactly what would happen
/// without a cache at all.
pub struct ProbeCache {
    pub(super) path: Option<PathBuf>,
    pub(super) now: u64,
    pub(super) file: ProbeCacheFile,
    pub(super) dirty: bool,
}

impl ProbeCache {
    /// Reads `<state>/probes/<repo_slug>.json`. `now` is stored for TTL
    /// comparisons and for stamping any entry this instance goes on to
    /// (re)probe.
    pub fn load(state: &super::super::state::StateDir, repo_slug: &str, now: u64) -> Self {
        let path = state.probes().join(format!("{repo_slug}.json"));
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ProbeCacheFile>(&text).ok())
            .filter(|file| file.version == PROBE_CACHE_VERSION)
            .unwrap_or_default();
        Self {
            path: Some(path),
            now,
            file,
            dirty: false,
        }
    }

    pub(super) fn key(name: &str, program: &str, bin: Option<&str>) -> String {
        format!("{name}\u{1}{program}\u{1}{}", bin.unwrap_or(""))
    }

    /// The cached verdict for `key` when both present and inside
    /// `PROBE_CACHE_TTL_SECS`; otherwise runs `probe`, records the result
    /// (unless it is `Unknown` -- an inconclusive probe is never frozen into
    /// the cache, so the very next call gets a fresh look rather than a
    /// stale guess), and returns it.
    pub(super) fn get_or_probe(&mut self, key: &str, probe: impl FnOnce() -> Liveness) -> Liveness {
        if let Some(cached) = self.file.entries.get(key)
            && self.now.saturating_sub(cached.checked_at) <= PROBE_CACHE_TTL_SECS
        {
            return if cached.live {
                Liveness::Live
            } else {
                Liveness::Absent(cached.reason.clone())
            };
        }
        let verdict = probe();
        match &verdict {
            Liveness::Live => {
                self.file.entries.insert(
                    key.to_string(),
                    CachedProbe {
                        checked_at: self.now,
                        live: true,
                        reason: String::new(),
                    },
                );
                self.dirty = true;
            }
            Liveness::Absent(reason) => {
                self.file.entries.insert(
                    key.to_string(),
                    CachedProbe {
                        checked_at: self.now,
                        live: false,
                        reason: reason.clone(),
                    },
                );
                self.dirty = true;
            }
            Liveness::Unknown(_) => {}
        }
        verdict
    }

    /// Best-effort, the same shape `score.rs::save_checkpoint` already uses:
    /// write to a process-unique temp file, then rename into place, so a
    /// process killed mid-write leaves the previous cache file intact rather
    /// than a truncated one. A no-op when nothing changed (`load` alone
    /// never dirties the cache) or when this instance has no backing path at
    /// all (`path: None`, the cacheless shape this module's own tests
    /// construct directly).
    pub fn save(&self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        let file = ProbeCacheFile {
            version: PROBE_CACHE_VERSION,
            entries: self.file.entries.clone(),
        };
        let Ok(json) = serde_json::to_string(&file) else {
            return;
        };
        let Some(dir) = path.parent() else {
            return;
        };
        let _ = super::super::state::create_private_dir_all(dir);
        let staged = dir.join(format!("{}.tmp", std::process::id()));
        if super::super::state::write_private(&staged, &json).is_ok() {
            let _ = std::fs::rename(&staged, path);
        }
    }
}

/// Recursively collects every file under `dir` for which `matches` is true.
fn collect_matching_files(dir: &Path, matches: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_matching_files(&path, matches, out);
        } else if matches(&path) {
            out.push(path);
        }
    }
}

/// Wave 3 (issues #390-#392): the shared "harness mints its own unpredictable
/// session directory/id" discovery shape for an adapter whose transcript root
/// is known but whose per-session subdirectory naming is not fully verified
/// (grok's url-encoded cwd segment, kimi's md5-of-cwd segment, cursor's
/// project-hash segment) -- the sibling of `codex::resolve_rollout`, but with
/// no verified per-line content to cross-check a session's own cwd against,
/// so this resolves by mtime alone: the NEWEST matching file with an mtime at
/// or after `floor_ms`. Two sessions of the SAME harness started in the SAME
/// repo within one poll cannot be told apart -- the same class of residual
/// `opencode::pinned_session_id`'s own doc comment already discloses for its
/// own harness.
fn newest_matching_file_since(
    root: &Path,
    floor_ms: u64,
    matches: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_matching_files(root, matches, &mut files);
    files
        .into_iter()
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            let ms = modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            (ms >= floor_ms).then_some((ms, path))
        })
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, path)| path)
}

/// Pins [`newest_matching_file_since`]'s answer under
/// `state.rollouts()/<adapter_name>-<short>.path`, mirroring
/// `codex::CodexAdapter::pinned_rollout` exactly: read the pin first (a
/// stable answer, no rescanning, and no drift if a newer session of the same
/// harness starts in the same repo mid-poll), else resolve and best-effort
/// write it. `None` when no `StateDir` resolves, this session has no
/// registered start time yet, or nothing under `root` matches -- the caller
/// falls back to its own "not found yet" path.
pub(crate) fn pin_newest_transcript(
    state: &super::super::state::StateDir,
    session: &SessionRef,
    adapter_name: &str,
    root: &Path,
    matches: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let short = super::super::sessions::short_id(session.id.as_str());
    let pin = state
        .rollouts()
        .join(format!("{adapter_name}-{short}.path"));
    if let Ok(recorded) = std::fs::read_to_string(&pin) {
        let recorded = PathBuf::from(recorded.trim());
        if recorded.is_file() {
            return Some(recorded);
        }
    }
    let record = super::super::sessions::load_record(state, &short)?;
    let floor_ms = record.started_at.saturating_mul(1_000);
    let resolved = newest_matching_file_since(root, floor_ms, matches)?;
    if super::super::state::create_private_dir_all(&state.rollouts()).is_ok() {
        let _ = super::super::state::write_private(&pin, &resolved.display().to_string());
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_emits_line_for_live_and_unknown_but_not_absent() {
        assert!(Liveness::Live.emits_line());
        assert!(Liveness::Unknown("inconclusive".to_string()).emits_line());
        assert!(!Liveness::Absent("no such binary".to_string()).emits_line());
    }

    /// The widened probe finds codex's real Windows install root even
    /// though it is nowhere on `PATH` -- CLAUDE.md's own "This Windows dev
    /// machine" note: `program_is_present` alone reports absent, but
    /// `program_is_present_widened`/`liveness_probe` widen the search and
    /// report `Live`.
    #[test]
    fn liveness_probe_finds_codex_via_its_known_install_root_when_absent_from_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let root = home.path().join("AppData/Local/Programs/OpenAI/Codex/bin");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("codex"), "").expect("write stub");
        let empty_path_dir = tempfile::tempdir().expect("tempdir");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(empty_path_dir.path().to_str().expect("utf8 path")),
        )]);

        assert!(
            !program_is_present("codex"),
            "the plain PATH-only check must not see it"
        );
        assert_eq!(liveness_probe("codex", "codex"), Liveness::Live);
        assert!(
            known_install_roots("claude").is_empty(),
            "claude has no known install root of its own, so this stub must not leak into it"
        );
    }

    /// A `PATH` entry that turns out to be a plain file, not a directory,
    /// makes the probe genuinely unable to tell -- `Unknown`, never a
    /// confirmed `Absent`.
    #[test]
    fn liveness_probe_is_unknown_when_a_path_entry_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_directory = dir.path().join("not-a-directory-file");
        std::fs::write(&not_a_directory, "").expect("write");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(not_a_directory.to_str().expect("utf8 path")),
        )]);

        let verdict = liveness_probe("claude", "claude");
        assert!(
            matches!(verdict, Liveness::Unknown(_)),
            "an inconclusive PATH entry must never be reported as a confirmed absence: \
             {verdict:?}"
        );
    }

    /// Acceptance criterion: a repo-layer config change cannot flip an
    /// adapter from `Absent` to `Live`. True by construction --
    /// `known_install_roots`/`liveness_probe` take no `CtxConfig`/repo path
    /// at all -- pinned down behaviourally: codex's line is identical
    /// whether the repo carries no `.zirv/.settings.toml` or a real,
    /// non-empty one, as long as its known install root holds a stub.
    #[test]
    fn known_install_root_liveness_does_not_depend_on_any_repo_config() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let root = home.path().join("AppData/Local/Programs/OpenAI/Codex/bin");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("codex"), "").expect("write stub");
        let empty_path_dir = tempfile::tempdir().expect("tempdir");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(empty_path_dir.path().to_str().expect("utf8 path")),
        )]);

        let plain_repo = tempfile::tempdir().expect("tempdir");
        let configured_repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(configured_repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            configured_repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\ncapacity = \"small\"\n",
        )
        .expect("write");
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        for repo in [plain_repo.path(), configured_repo.path()] {
            let cfg = CtxConfig {
                agents: crate::settings::AgentGate::load(repo, &|k| empty.get(k).cloned())
                    .expect("load"),
                ..CtxConfig::default()
            };
            let lines = harness_prompt_lines(&cfg, "");
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("- codex:") && l.contains("zirv agent codex")),
                "codex must be live via its known install root regardless of repo config \
                 ({}): {lines:?}",
                repo.display()
            );
        }
    }

    #[test]
    fn probe_cache_get_or_probe_reuses_a_cached_verdict_without_reprobing() {
        let mut cache = ProbeCache {
            path: None,
            now: 1_000,
            file: ProbeCacheFile::default(),
            dirty: false,
        };
        let calls = std::cell::Cell::new(0);
        let first = cache.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Live
        });
        assert_eq!(first, Liveness::Live);
        assert_eq!(calls.get(), 1);

        let second = cache.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Absent("should not run".to_string())
        });
        assert_eq!(
            second,
            Liveness::Live,
            "the cached verdict must be reused, not a fresh probe"
        );
        assert_eq!(
            calls.get(),
            1,
            "a second call within the TTL must not re-probe"
        );
    }

    #[test]
    fn probe_cache_reprobes_after_the_ttl_elapses() {
        let mut cache = ProbeCache {
            path: None,
            now: 1_000,
            file: ProbeCacheFile::default(),
            dirty: false,
        };
        let _ = cache.get_or_probe("k", || Liveness::Live);

        cache.now += PROBE_CACHE_TTL_SECS + 1;
        let calls = std::cell::Cell::new(0);
        let verdict = cache.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Absent("gone".to_string())
        });
        assert_eq!(calls.get(), 1, "past the TTL, get_or_probe must re-probe");
        assert_eq!(verdict, Liveness::Absent("gone".to_string()));
    }

    /// An inconclusive probe must never be frozen into the cache: the very
    /// next call gets a fresh look rather than a stale guess.
    #[test]
    fn probe_cache_never_freezes_an_unknown_verdict() {
        let mut cache = ProbeCache {
            path: None,
            now: 1_000,
            file: ProbeCacheFile::default(),
            dirty: false,
        };
        let first = cache.get_or_probe("k", || Liveness::Unknown("inconclusive".to_string()));
        assert_eq!(first, Liveness::Unknown("inconclusive".to_string()));
        assert!(
            !cache.dirty,
            "an Unknown verdict must never be written to the cache"
        );

        let calls = std::cell::Cell::new(0);
        let second = cache.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Live
        });
        assert_eq!(
            calls.get(),
            1,
            "an Unknown verdict must not be cached, so the next call re-probes"
        );
        assert_eq!(second, Liveness::Live);
    }

    #[test]
    fn probe_cache_save_and_load_round_trips_a_live_verdict() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state =
            crate::commands::ctx::state::StateDir::from_root(state_dir.path().to_path_buf());

        let mut cache = ProbeCache::load(&state, "some-repo", 1_000);
        let first = cache.get_or_probe("k", || Liveness::Live);
        assert_eq!(first, Liveness::Live);
        cache.save();

        let mut reloaded = ProbeCache::load(&state, "some-repo", 1_000);
        let calls = std::cell::Cell::new(0);
        let second = reloaded.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Absent("should not run".to_string())
        });
        assert_eq!(
            second,
            Liveness::Live,
            "a saved verdict must be read back on the next load"
        );
        assert_eq!(calls.get(), 0, "a warm cache must not re-probe");
    }

    #[test]
    fn probe_cache_load_with_no_file_yet_probes_fresh() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state =
            crate::commands::ctx::state::StateDir::from_root(state_dir.path().to_path_buf());

        let mut cache = ProbeCache::load(&state, "never-seen-repo", 1_000);
        let calls = std::cell::Cell::new(0);
        let verdict = cache.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Live
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(verdict, Liveness::Live);
    }

    /// A cache file that is not valid JSON (or not this version) must read
    /// back empty, never error: a cache miss is exactly as if the cache did
    /// not exist.
    #[test]
    fn probe_cache_load_ignores_an_unparseable_file() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state =
            crate::commands::ctx::state::StateDir::from_root(state_dir.path().to_path_buf());
        std::fs::create_dir_all(state.probes()).expect("mkdir");
        std::fs::write(state.probes().join("repo.json"), "not json").expect("write");

        let mut cache = ProbeCache::load(&state, "repo", 1_000);
        let calls = std::cell::Cell::new(0);
        let verdict = cache.get_or_probe("k", || {
            calls.set(calls.get() + 1);
            Liveness::Live
        });
        assert_eq!(
            calls.get(),
            1,
            "an unparseable cache file must not error out, just re-probe"
        );
        assert_eq!(verdict, Liveness::Live);
    }

    /// M7 probed the adapter's own program while `wrap` spawned the user's
    /// argv, so the file flag could be handed to a binary that never
    /// advertised it -- failing the launch outright, which is the one thing
    /// the probe promises never to do. The probe target now comes from the
    /// argv about to be spawned, which means finding the invocation in it.
    #[test]
    fn the_program_invocation_stops_at_the_first_flag() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };

        assert_eq!(
            program_invocation(&argv(&["claude", "-p", "task"])),
            Some(("claude".to_string(), vec![]))
        );
        assert_eq!(
            program_invocation(&argv(&["/usr/bin/env", "claude", "-p", "task"])),
            Some(("/usr/bin/env".to_string(), vec!["claude".to_string()]))
        );
        assert_eq!(
            program_invocation(&argv(&["sh", "/opt/wrap.sh", "--model", "opus"])),
            Some(("sh".to_string(), vec!["/opt/wrap.sh".to_string()]))
        );
        assert_eq!(program_invocation(&[]), None, "nothing to probe");
    }

    /// Off Windows there is nothing to rewrite, and on Windows a program that
    /// is already directly executable is spawned exactly as it was written.
    #[test]
    fn a_directly_executable_program_is_left_alone() {
        let resolved = resolve_program("claude").expect("resolvable");
        assert_eq!(resolved.program, "claude");
        assert!(
            resolved.prefix.is_empty() || cfg!(windows),
            "only Windows ever inserts a launcher"
        );

        let missing = resolve_program("definitely-not-a-program-anywhere").expect("no error");
        assert_eq!(
            missing,
            ResolvedProgram::direct("definitely-not-a-program-anywhere"),
            "a program that resolves to nothing keeps the OS's own not-found"
        );
    }

    /// The npm install layout: `claude` on `PATH` is `claude.cmd`, which
    /// `CreateProcessW` rejects outright. `PATHEXT` finds it, and `cmd.exe`
    /// is what can actually run it.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_rewritten_to_run_through_cmd_exe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("shim-agent.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write");

        let resolved = resolve_program(&shim.display().to_string()).expect("resolvable");
        assert!(
            resolved.program.to_lowercase().contains("cmd"),
            "got {}",
            resolved.program
        );
        assert_eq!(
            resolved.prefix,
            vec!["/c".to_string(), shim.display().to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_powershell_script_is_rewritten_to_run_through_powershell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("shim-agent.ps1");
        std::fs::write(&script, "exit 0\r\n").expect("write");

        let resolved = resolve_program(&script.display().to_string()).expect("resolvable");
        assert_eq!(resolved.program, "powershell");
        assert_eq!(
            resolved.prefix,
            vec![
                "-NoProfile".to_string(),
                "-File".to_string(),
                script.display().to_string()
            ]
        );
    }

    /// A bare name resolved off `PATH` is one the shell itself claimed to be
    /// executable, so a file type with no launcher is a failure zirv can name
    /// before spawning instead of letting it surface as `os error 193`. A
    /// program written with a directory in it is the caller's own choice and
    /// is never an error here, whatever it ends in.
    #[cfg(windows)]
    #[test]
    fn an_unlaunchable_program_on_path_is_named_rather_than_left_to_error_193() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("shim-agent.py");
        std::fs::write(&script, "print('x')\n").expect("write");

        assert_eq!(
            resolve_program(&script.display().to_string()),
            Ok(ResolvedProgram::direct(&script.display().to_string())),
            "an explicit path is the caller's own choice"
        );

        // Temporarily put the directory on PATH so the bare name resolves the
        // way the shell would, with `.PY` advertised on PATHEXT.
        //
        // NEW-1: a guard, not a manual restore. The restore used to sit after
        // an `expect_err`, so a failing resolution left this process with a
        // mangled `PATH` and a `PATHEXT` of `.EXE;.CMD;.PY` -- the highest
        // blast radius of any leak in the suite, since every later test that
        // spawns anything resolves its program through both.
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);
        let err = resolve_program("shim-agent").expect_err("no launcher for .py");

        assert!(err.contains("shim-agent.py"), "the error names it: {err}");
        assert!(err.contains("shim-agent"), "and what was asked for: {err}");
    }

    /// FIX 2a: a `cmd.exe /c <shim>` launch whose downstream arguments carry a
    /// cmd.exe metacharacter is refused, because cmd.exe re-parses that
    /// character as a command rather than passing it through to the shim. This
    /// is the RCE-closing guard, tested as a pure decision function -- no
    /// process is spawned.
    #[cfg(windows)]
    #[test]
    fn a_shim_form_launch_with_a_metachar_arg_is_refused() {
        let args = vec![
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "-p".to_string(),
            "foo&calc".to_string(),
        ];
        let err = guard_cmd_shim_reparse("cmd.exe", &args)
            .expect_err("a metachar after the shim path is command injection");
        assert!(
            err.contains("foo&calc"),
            "the error names the offending arg: {err}"
        );

        // A full-path, upper-cased COMSPEC is recognised structurally too.
        assert!(
            guard_cmd_shim_reparse(
                "C:\\Windows\\System32\\CMD.EXE",
                &[
                    "/C".to_string(),
                    "claude.cmd".to_string(),
                    "\"; calc; \"".to_string(),
                ],
            )
            .is_err(),
            "an embedded quote (the BatBadBut toggle) is rejected regardless of cmd casing"
        );
    }

    /// FIX 2a: the two shim-prefix tokens (`/c` and the shim path) are
    /// zirv-controlled and never trip the guard, and a clean downstream arg --
    /// including a real Bedrock model id with `:` `/` `.` -- passes. Runs on
    /// every platform: off Windows it exercises the no-op path, on Windows the
    /// real allow decision.
    #[test]
    fn a_shim_form_launch_with_only_clean_args_is_allowed() {
        let args = vec![
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "-p".to_string(),
            "do the thing".to_string(),
            "--model".to_string(),
            "us.anthropic.claude-sonnet-4-v1:0".to_string(),
        ];
        assert!(guard_cmd_shim_reparse("cmd.exe", &args).is_ok());
    }

    /// FIX D (defense-in-depth): the `powershell -NoProfile -File <script>`
    /// launcher form is guarded the same way as the cmd shim -- everything
    /// through the `-File <script>` pair is zirv-controlled prefix, and a
    /// metacharacter in a token after it is refused. The two prefix tokens and
    /// the script path never trip it.
    #[cfg(windows)]
    #[test]
    fn a_powershell_file_launch_is_guarded_after_the_script_path() {
        let bad = vec![
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "foo&calc".to_string(),
        ];
        assert!(
            guard_cmd_shim_reparse("powershell", &bad).is_err(),
            "a metachar after the script path is refused"
        );

        let clean = vec![
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "do the thing".to_string(),
        ];
        assert!(
            guard_cmd_shim_reparse("pwsh", &clean).is_ok(),
            "clean args on the powershell form pass, and the prefix never trips"
        );
    }

    /// FIX 2a: a direct `.exe` (no cmd.exe launcher prefix) is not the shim
    /// form, so the guard is a no-op even for an argument that would be
    /// dangerous through cmd.exe -- `CreateProcess` receives it as a literal.
    /// This is also what keeps the test harness's own `sh <script>` fake agents
    /// from being rejected.
    #[test]
    fn a_non_shim_launch_is_never_guarded() {
        let args = vec!["-p".to_string(), "foo&calc".to_string()];
        assert!(guard_cmd_shim_reparse("claude.exe", &args).is_ok());
        assert!(guard_cmd_shim_reparse("/opt/homebrew/bin/claude", &args).is_ok());
        assert!(guard_cmd_shim_reparse("sh", &["/tmp/fake-agent.sh".to_string()]).is_ok());
    }

    /// FINDING 3: an argv that is *already resolved* to the `cmd.exe /c <shim>`
    /// launcher form (what the interactive path hands `injection_args_for_
    /// session`) is recognised as reparsing, where re-resolving the literal
    /// head `cmd.exe` would have found a plain `.exe` and missed it. A direct
    /// `.exe` argv is not a launcher form and is not flagged.
    #[cfg(windows)]
    #[test]
    fn an_already_resolved_launcher_argv_is_recognised_as_reparsing() {
        let resolved_cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "C:\\tools\\claude.cmd".to_string(),
            "the prompt".to_string(),
        ];
        assert!(launch_reparses_through_shim(&resolved_cmd));

        let resolved_ps = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-File".to_string(),
            "C:\\tools\\agent.ps1".to_string(),
            "arg".to_string(),
        ];
        assert!(launch_reparses_through_shim(&resolved_ps));

        let direct = vec!["C:\\tools\\claude.exe".to_string(), "--resume".to_string()];
        assert!(!launch_reparses_through_shim(&direct));
    }

    /// Off Windows there is no launcher reparse, so the detection is always
    /// `false` -- including for an argv that structurally looks like one.
    #[cfg(not(windows))]
    #[test]
    fn launch_reparse_detection_is_a_noop_off_windows() {
        let looks_like_cmd = vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "claude.cmd".to_string(),
        ];
        assert!(!launch_reparses_through_shim(&looks_like_cmd));
        assert!(!launch_reparses_through_shim(&[]));
    }

    #[test]
    fn format_launch_error_recognizes_notfound_errors() {
        // NotFound io::Error
        let notfound_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let result = format_launch_error(&notfound_err, "claude", "claude");
        assert!(result.contains("claude"));
        assert!(result.contains("not found"));
        assert!(result.contains("Install"));

        // PermissionDenied io::Error
        let perm_err =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let result = format_launch_error(&perm_err, "codex", "codex");
        assert!(result.contains("codex"));
        assert!(result.contains("failed to start"));
    }

    // -- issue #690 (remaining scope): the launch pre-flight ------------------

    /// The pre-flight's refusal must be the same sentence the spawn's own
    /// `NotFound` would have produced, to the byte -- it turns a slow failure
    /// into a fast one and nothing else, so an operator who has seen the slow
    /// one must not have to decide whether the fast one means something
    /// different.
    #[test]
    fn the_launch_preflight_refuses_with_the_spawns_own_not_found_wording() {
        let cfg = super::super::tests::permissive_cfg();
        let adapter = select_with_presence(Some("claude"), &[], &cfg, true, &only_installed(&[]))
            .expect("naming an adapter never consults presence");

        let err = refuse_if_program_absent_with_presence(
            adapter.as_ref(),
            &cfg,
            &only_installed(&["codex"]),
        )
        .expect_err("a confidently absent harness must not reach pacing");

        let from_the_spawn = format_launch_error(
            &std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
            adapter.name(),
            adapter.program(),
        );
        assert_eq!(err.to_string(), from_the_spawn);
    }

    /// Fail-open, the discipline [`Liveness`] and [`program_is_present`]
    /// already hold every other presence consumer to: a probe that reached no
    /// verdict leaves the launch behaving exactly as it did before a
    /// pre-flight existed.
    #[test]
    fn the_launch_preflight_lets_an_undecidable_probe_launch() {
        let cfg = super::super::tests::permissive_cfg();
        let adapter = select_with_presence(Some("claude"), &[], &cfg, true, &nothing_decidable())
            .expect("claude resolves");

        refuse_if_program_absent_with_presence(adapter.as_ref(), &cfg, &nothing_decidable())
            .expect("Unknown must never cost a launch that would have worked");
    }

    /// The other half of fail-open, and the ordinary case: an installed
    /// harness is waved straight through.
    #[test]
    fn the_launch_preflight_lets_an_installed_harness_launch() {
        let cfg = super::super::tests::permissive_cfg();
        let adapter =
            select_with_presence(Some("claude"), &[], &cfg, true, &everything_installed())
                .expect("claude resolves");

        refuse_if_program_absent_with_presence(
            adapter.as_ref(),
            &cfg,
            &only_installed(&["claude"]),
        )
        .expect("an installed harness launches");
    }

    /// `resolve_default_with_presence`'s own `consult_presence = bin.
    /// is_none()` rule, reused rather than re-invented: an operator-set
    /// `agent_bin` need not be a path a `stat` can answer (the `sh
    /// <wrapper>.sh` shape below is this codebase's own fixture convention
    /// and resolves to nothing on disk), so the pre-flight must not probe it
    /// at all. Proven by an oracle that panics if it is ever consulted --
    /// "returned Ok" alone would also be satisfied by a probe that ran and
    /// happened to answer `Live`.
    #[test]
    fn the_launch_preflight_never_probes_while_agent_bin_is_set() {
        let cfg = CtxConfig {
            agent_bin: Some("sh /nowhere/wrapper.sh".to_string()),
            ..CtxConfig::default()
        };
        let adapter =
            select_with_presence(Some("claude"), &[], &cfg, true, &everything_installed())
                .expect("claude resolves");
        let never: &dyn Fn(&str, &str) -> Liveness = &|_name: &str, _program: &str| -> Liveness {
            panic!("an operator-set agent_bin must never be probed")
        };

        refuse_if_program_absent_with_presence(adapter.as_ref(), &cfg, never)
            .expect("an agent_bin override is an operator choice, not a presence question");
    }

    /// Never substitute: the pre-flight only ever makes a failure arrive
    /// sooner. A configured `agent` naming a harness this machine does not
    /// have fails under *that* harness's own name, and the installed one is
    /// never quietly put in its place.
    #[test]
    fn the_launch_preflight_names_the_configured_harness_and_never_switches_it() {
        let cfg = CtxConfig {
            agent: Some("codex".to_string()),
            ..CtxConfig::default()
        };
        let (adapter, _origin) = resolve_default_with_presence(&cfg, &only_installed(&["claude"]))
            .expect("a configured agent is never re-chosen by presence");
        assert_eq!(adapter.name(), "codex");

        let err = refuse_if_program_absent_with_presence(
            adapter.as_ref(),
            &cfg,
            &only_installed(&["claude"]),
        )
        .expect_err("codex is not installed on this stated machine");
        let message = err.to_string();
        assert!(
            message.contains("adapter 'codex'"),
            "the refusal must name the harness the operator asked for: {message}"
        );
        assert!(
            !message.contains("claude"),
            "the installed harness must never appear as a substitute: {message}"
        );
    }
}
