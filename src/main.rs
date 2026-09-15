use std::io::IsTerminal;
use std::path::Path;

use clap::Parser;
use commands::ctx;
use commands::setup;
use commands::workflow;
use commands::{
    create::{CreateOptions, create_script},
    help::show_help,
    init::init_zirv,
    report, update,
    version::get_version,
};

mod commands;
mod input;
mod output;
mod script_runner;
mod settings;
mod skill;
mod style;
mod utils;

use input::Input;
use script_runner::execute;
use utils::file_to_script;

/// True when the top-level invocation is exactly `zirv --help`/`zirv -h`,
/// i.e. the help flag stands in for the command itself rather than being a
/// script's own parameter. Checked against raw argv, before clap parses
/// anything, so clap's auto-generated help for the `Input` struct never
/// fires and `zirv help`'s rich script listing is shown instead.
fn is_top_level_help(argv: &[String]) -> bool {
    matches!(argv.get(1).map(String::as_str), Some("--help") | Some("-h"))
}

/// True when argv[1] names the `ctx` built-in, compared **case-insensitively**
/// to match `utils::is_reserved_command`/`RESERVED_COMMANDS`: on NTFS/APFS a
/// script file `Ctx.yaml` resolves the same as `ctx.yaml`, so a case-sensitive
/// interception would let `zirv Ctx` fall through to script lookup and run a
/// repo `.zirv/Ctx.yaml` that `zirv help` simultaneously reports as shadowed
/// and unreachable. `dispatch` skips argv[1] itself, so the exact casing the
/// user typed never reaches the verb tree.
fn is_top_level_ctx(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|s| s.eq_ignore_ascii_case("ctx"))
}

/// Workflow commands have their own clap tree and must be intercepted before
/// the legacy script runner resolves a same-named file under `.zirv/`.
fn is_top_level_workflow_command(argv: &[String]) -> bool {
    argv.get(1).is_some_and(|name| {
        workflow::TOP_LEVEL_COMMANDS
            .iter()
            .any(|command| name.eq_ignore_ascii_case(command))
    })
}

/// True when argv[1] names the `memory` built-in, compared case-insensitively
/// like the `ctx` check above, so a repo `.zirv/Memory.yaml` (or any
/// differently-cased script) can never shadow it. Unlike `chat`/`agent`
/// below, `memory` is not a 1:1 alias into a single `ctx` verb: it has its
/// own verb tree (`init`/`status`/`list`/`recall`/`remember`/`forget`/`verify`),
/// dispatched by `commands::ctx::memory_cli::dispatch` directly rather than
/// through `ctx::dispatch`.
fn is_top_level_memory(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|s| s.eq_ignore_ascii_case("memory"))
}

/// True when argv[1] names the `session` built-in (issue #352): the
/// persistent runtime's own verb tree, intercepted here for the same reason
/// `memory` and `context` are -- it is a command family of its own, not a
/// verb under `ctx`, and a repo `.zirv/Session.yaml` must never shadow it.
fn is_top_level_session(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|s| s.eq_ignore_ascii_case("session"))
}

fn is_top_level_setup(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|name| name.eq_ignore_ascii_case("setup"))
}

/// `zirv report` owns its own clap tree and must be intercepted before a
/// repository script can resolve under the same reserved name. The
/// case-insensitive match mirrors every other raw-argv built-in.
fn is_top_level_report(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|name| name.eq_ignore_ascii_case("report"))
}

fn is_top_level_update(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|name| name.eq_ignore_ascii_case("update"))
}

/// True when argv[1] names the `context` built-in (issue #45, "Context
/// 7/8"): a top-level command family sibling to `ctx`, not one of its verbs
/// (see `commands::ctx::context_cli`'s module doc for why), so it gets its
/// own interception here rather than routing through `ctx::dispatch`.
/// Case-insensitive for the same reason `is_top_level_ctx` is: NTFS/APFS
/// resolve a script file `Context.yaml` the same as `context.yaml`, and
/// `.zirv/context/` (the canonical instruction directory `context.rs` reads)
/// already sits at this exact name -- both are guarded the same way
/// `utils::RESERVED_COMMANDS` guards every other built-in.
fn is_top_level_context(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|s| s.eq_ignore_ascii_case("context"))
}

/// Whether argv requests the bundled operator skill (issue #355): either the
/// top-level `--skill` flag or a bare `zirv skill` with no further argument.
/// `zirv skill list`/`zirv skill show <id>` remain the pre-existing
/// `workflow::skill` engineering-skill inspection tree (see that module's
/// own doc comment) and fall through to `is_top_level_workflow_command`'s
/// dispatch unchanged: a bare `zirv skill` already failed to parse there,
/// since `SkillArgs::command` is a required subcommand, so repurposing that
/// one previously-erroring shape here breaks no working invocation.
/// Case-insensitive for the bare-word form, matching every other reserved
/// built-in. Returns whether `--json` was also requested; `None` when argv
/// names neither form (including a `skill`/`--skill` invocation with any
/// other trailing argument, which is left to fall through).
fn top_level_skill_request(argv: &[String]) -> Option<bool> {
    let names_skill = argv.get(1).map(String::as_str) == Some("--skill")
        || argv.get(1).is_some_and(|s| s.eq_ignore_ascii_case("skill"));
    if !names_skill {
        return None;
    }
    match argv.get(2).map(String::as_str) {
        None => Some(false),
        Some("--json") if argv.len() == 3 => Some(true),
        _ => None,
    }
}

/// Whether argv requests the generated command surface (issue #355):
/// `zirv commands [--json]`, matched case-insensitively like every other
/// reserved built-in. `None` for anything else, including a trailing
/// argument this built-in does not recognize -- there is no further verb
/// tree to fall through to here, so that case is left to the ordinary
/// script-lookup "not found" error.
fn top_level_commands_request(argv: &[String]) -> Option<bool> {
    if !argv
        .get(1)
        .is_some_and(|s| s.eq_ignore_ascii_case("commands"))
    {
        return None;
    }
    match argv.get(2).map(String::as_str) {
        None => Some(false),
        Some("--json") if argv.len() == 3 => Some(true),
        _ => None,
    }
}

/// `zirv chat` and `zirv agent` are top-level aliases for `zirv ctx chat`
/// and `zirv ctx agent`, checked against raw argv (like the `ctx`
/// interception above) so they run before clap ever sees `Input`. Returns
/// the ctx verb name to route to, or `None` when argv doesn't name one of
/// these aliases in the command slot. Matched **case-insensitively**, the
/// same rule `is_top_level_ctx`/`is_reserved_command` use, so `zirv Chat`
/// cannot slip past into a repo `Chat.yaml` script.
fn top_level_ctx_alias(argv: &[String]) -> Option<&'static str> {
    match argv.get(1).map(String::as_str) {
        Some(s) if s.eq_ignore_ascii_case("chat") => Some("chat"),
        Some(s) if s.eq_ignore_ascii_case("agent") => Some("agent"),
        _ => None,
    }
}

/// Rewrites argv for a top-level alias into the shape `ctx::dispatch`
/// expects: `args[0]` literally `"ctx"` (matching how the raw `ctx`
/// interception already calls it), the resolved verb, then whatever
/// followed the alias on the command line. Reusing `ctx::dispatch` this way
/// means the alias gets the full ctx verb tree for free: subcommand
/// parsing, `--help` exiting 0, and the same parse-failure classification.
fn rewrite_ctx_alias_args(verb: &str, argv: &[String]) -> Vec<String> {
    let mut args = vec!["ctx".to_string(), verb.to_string()];
    args.extend(argv.iter().skip(2).cloned());
    args
}

/// Issue #540: `zirv native` is a thin, case-insensitive top-level alias for
/// `zirv chat --runtime native`, matched against raw argv exactly like
/// `top_level_ctx_alias` above so it runs before clap ever sees `Input`.
/// Reserved case-insensitively in `utils::RESERVED_COMMANDS` too, so a
/// same-named script or shortcut can never shadow it.
fn is_top_level_native_alias(argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|s| s.eq_ignore_ascii_case("native"))
}

/// True for `zirv native --help`/`-h` (case-insensitive on `native`,
/// matching `is_top_level_native_alias`; the flag itself is not case-folded,
/// the same convention `is_top_level_help` already uses for `--help`/`-h`).
/// Matches `--help`/`-h` ANYWHERE in the trailing argv, not only as the sole
/// token -- `zirv native --foo --help` and `zirv native --help extra` are
/// both a help request, the same way clap itself treats `--help` as
/// short-circuiting regardless of what else is on the line. Stops looking at
/// a bare `--`: everything after that separator is opaque extra argv for the
/// adapter (`ChatArgs::extra`), not a flag for this alias to interpret, so a
/// literal `--help` string an operator meant to hand to the harness itself
/// must not be swallowed here.
///
/// Intercepted before the ordinary argv rewrite below so `zirv native
/// --help` prints its own experimental-labelled prose
/// (`ctx::chat::native_help_text`) rather than clap's generated help for the
/// ordinary `chat` verb tree, which says nothing about any of this.
fn is_top_level_native_help(argv: &[String]) -> bool {
    if !is_top_level_native_alias(argv) {
        return false;
    }
    for arg in &argv[2..] {
        if arg == "--" {
            return false;
        }
        if arg == "--help" || arg == "-h" {
            return true;
        }
    }
    false
}

/// Review finding (issue #540): `zirv native` always selects the native
/// runtime by splicing `--runtime native` onto the FRONT of the rewritten
/// argv (see `rewrite_native_alias_args` below) -- but `ChatArgs::runtime`
/// is a plain `Option<String>`, and clap lets a LATER `--runtime` occurrence
/// win over an earlier one. Without this guard, `zirv native --runtime
/// harness` rewrote to `ctx chat --runtime native --runtime harness`, clap
/// resolved `args.runtime` to `Some("harness")`, and `run_with` silently
/// launched the ordinary wrapped harness -- exactly the "silently select a
/// legacy runtime" the issue's own contract forbids. Checked against the raw
/// trailing argv, both spellings (`--runtime harness` and
/// `--runtime=harness`), anywhere before a `--` separator (matching
/// `is_top_level_native_help`'s own boundary: past `--` is opaque extra argv
/// for the adapter, not a flag for this alias).
fn native_alias_has_user_supplied_runtime(argv: &[String]) -> bool {
    if !is_top_level_native_alias(argv) {
        return false;
    }
    for arg in &argv[2..] {
        if arg == "--" {
            return false;
        }
        if arg == "--runtime" || arg.starts_with("--runtime=") {
            return true;
        }
    }
    false
}

/// Rewrites `zirv native [args...]` into the `ctx chat --runtime native
/// [args...]` shape `ctx::dispatch` expects -- the same `args[0] == "ctx"`
/// convention `rewrite_ctx_alias_args` uses -- so `chat.rs`'s own
/// `run_with`/`run_native_chat` is the ONE code path that owns validation,
/// nesting, TTY, journal and pane startup for both spellings. Every argument
/// after `native` forwards unchanged, so a native-compatible `zirv chat`
/// option (and the existing native validator's exact refusal for a
/// wrapped-only one) behaves identically either way. Callers must check
/// `native_alias_has_user_supplied_runtime` first: this function does not
/// itself guard against a caller-supplied `--runtime` silently overriding
/// the one it splices in.
fn rewrite_native_alias_args(argv: &[String]) -> Vec<String> {
    let mut args = vec![
        "ctx".to_string(),
        "chat".to_string(),
        "--runtime".to_string(),
        "native".to_string(),
    ];
    args.extend(argv.iter().skip(2).cloned());
    args
}

/// What a bare `zirv` invocation (no arguments at all) does: open an
/// interactive chat when this looks like a zirv-managed repo and *both*
/// stdin and stdout are a real terminal, or fall back to the ordinary help
/// listing otherwise. A pure function of those three facts so the policy is
/// testable without a pty or a real filesystem/stdin/stdout check --
/// `zirv_dir_present` and `std::io::IsTerminal` supply them at the call site.
///
/// Both stdin *and* stdout have to be a terminal, not just stdin: `zirv |
/// less` (or any other redirect of stdout alone) has an interactive stdin
/// but would otherwise launch a chat session into a pipe nothing is reading
/// interactively.
///
/// This is a deliberate behavior change from clap's own bare-invocation
/// handling: `Input::command` is a required positional, so before this a
/// bare `zirv` was a clap usage error exiting 2. `BareTarget::Help` instead
/// exits 0, matching top-level `zirv --help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BareTarget {
    Chat,
    Help,
}

fn bare_invocation_target(
    zirv_dir_exists: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> BareTarget {
    if zirv_dir_exists && stdin_is_tty && stdout_is_tty {
        BareTarget::Chat
    } else {
        BareTarget::Help
    }
}

/// True when `./.zirv` (relative to `cwd`) exists as a directory. **Local
/// only**, deliberately: a global `~/.zirv` says something about the
/// operator's machine, not about the directory zirv happens to be run from,
/// and treating it as "this is a zirv-managed repo" would open a chat
/// session for a bare `zirv` in literally any directory for anyone who has
/// ever run `zirv create --global` once. Matches the approved design's own
/// wording -- "in a repo with a `.zirv` directory" -- and mirrors
/// `SCRIPT_DIR_NAME` resolution order elsewhere: local is checked, global is
/// not consulted for this decision. `zirv chat`/`zirv ctx chat` remain
/// unaffected and still work anywhere, with or without a local `.zirv`.
fn zirv_dir_present(cwd: &Path) -> bool {
    cwd.join(utils::SCRIPT_DIR_NAME).is_dir()
}

/// Whether the guided first-run setup wizard should run before continuing
/// into whatever command the operator actually typed: only in a real
/// interactive terminal (a script/CI invocation must never hang on a prompt),
/// and only when zirv has genuinely never been configured
/// (`setup::first_run_needed`). Pure so this policy -- as opposed to the
/// wizard's own interactive body, which cannot be automated -- is testable
/// without a pty or a real `HOME`.
fn first_run_wizard_should_run(
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    first_run_needed: bool,
) -> bool {
    stdin_is_tty && stdout_is_tty && first_run_needed
}

/// Runs the guided first-run wizard when appropriate, degrading on any
/// failure -- no `HOME`, a declined terminal check, Ctrl-C, any other
/// `dialoguer` error -- with a note on stderr rather than a silent swallow,
/// so an operator who answered every prompt still learns that nothing was
/// saved. The wizard must never make the command the operator typed worse,
/// so from a caller's perspective its only two outcomes are "configured,
/// keep going exactly as planned" and "didn't run (or aborted), keep going
/// exactly as before".
///
/// Checks are ordered cheapest first: the TTY flags are already in hand, so
/// they gate before anything that touches `HOME` or the filesystem. `chat`'s
/// own nesting guard (`ctx::sessions::nesting_refusal`) is checked before
/// ever launching the wizard, not just before the eventual `ctx::dispatch`:
/// a `zirv chat`/bare `zirv` run from inside an existing agent session (a
/// dashboard pane, a nested wrap) must refuse outright, the same as `chat`
/// itself would -- launching six interactive prompts first, only to refuse
/// the command anyway, would block on the first prompt in exactly the
/// nested-pty context the guard exists to protect.
///
/// `allow_nested` is always `false` at both call sites below: the bare-`zirv`
/// path has no argv to carry a flag on, and the `chat`-alias path only ever
/// reaches this function for literally `zirv chat` with no further
/// arguments (see that call site), so there is no `--allow-nested` to read
/// either -- an operator who passes it gets the ordinary `chat` verb's own
/// check instead, with the real flag.
fn maybe_run_first_run_wizard(stdin_is_tty: bool, stdout_is_tty: bool, allow_nested: bool) {
    if !stdin_is_tty || !stdout_is_tty {
        return;
    }
    let Ok(home) = utils::home_dir() else {
        return;
    };
    let needed = setup::first_run_needed(&home.join(utils::SCRIPT_DIR_NAME));
    if !first_run_wizard_should_run(stdin_is_tty, stdout_is_tty, needed) {
        return;
    }
    let env = ctx::config::env_from_process();
    if ctx::sessions::nesting_refusal("chat", &env, allow_nested).is_some() {
        return;
    }
    if let Err(e) = setup::run_first_run() {
        eprintln!(
            "zirv: first-run setup did not finish ({e}); continuing without it. \
             Run `zirv setup` to configure later."
        );
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if is_top_level_ctx(&argv) {
        std::process::exit(ctx::dispatch(&argv[1..]));
    }

    if is_top_level_context(&argv) {
        std::process::exit(ctx::context_cli::dispatch(&argv[1..]));
    }

    if let Some(json) = top_level_skill_request(&argv) {
        let version = env!("CARGO_PKG_VERSION");
        if json {
            match serde_json::to_string_pretty(&skill::to_json(version)) {
                Ok(text) => println!("{text}"),
                Err(e) => {
                    output::error(e);
                    std::process::exit(1);
                }
            }
        } else {
            println!("{}", skill::render(version));
        }
        return;
    }

    if let Some(json) = top_level_commands_request(&argv) {
        std::process::exit(commands::command_schema::dispatch(
            json,
            &mut std::io::stdout(),
        ));
    }

    if is_top_level_workflow_command(&argv) {
        std::process::exit(workflow::dispatch(&argv));
    }

    if is_top_level_memory(&argv) {
        std::process::exit(ctx::memory_cli::dispatch(&argv[1..]));
    }

    if is_top_level_session(&argv) {
        std::process::exit(ctx::session::dispatch(&argv[1..]));
    }

    if is_top_level_setup(&argv) {
        std::process::exit(setup::dispatch(&argv[1..]));
    }

    if is_top_level_report(&argv) {
        std::process::exit(report::dispatch(&argv[1..]));
    }

    if is_top_level_update(&argv) {
        std::process::exit(update::dispatch(&argv[1..]));
    }

    if let Some(verb) = top_level_ctx_alias(&argv) {
        // Only `chat` gets the first-run gate, not `agent`: `agent` delegates
        // one bounded task to another harness rather than opening the kind of
        // standing interactive session first-run setup is meant to precede.
        // And only a bare `zirv chat` with no further arguments qualifies --
        // `zirv chat --help` (or any other flag, valid or not) is a discovery
        // or parse-failure path that `ctx::dispatch` owns downstream; running
        // the wizard first would answer neither and write config besides.
        if verb == "chat" && argv.len() == 2 {
            maybe_run_first_run_wizard(
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
                false,
            );
        }
        std::process::exit(ctx::dispatch(&rewrite_ctx_alias_args(verb, &argv)));
    }

    if is_top_level_native_alias(&argv) {
        if is_top_level_native_help(&argv) {
            print!("{}", ctx::chat::native_help_text());
            return;
        }
        // Review finding (issue #540): a user-supplied `--runtime` would
        // otherwise be silently appended after the `--runtime native` this
        // rewrite splices in, and clap's `Option<String>` lets the LATER
        // occurrence win -- see `native_alias_has_user_supplied_runtime`'s
        // own doc comment for exactly how that let `zirv native --runtime
        // harness` launch the wrapped harness with no indication anything
        // was overridden. Refused outright, before the rewrite ever runs,
        // rather than left for `chat.rs` to notice (it can't: by the time
        // clap parses `ChatArgs`, only the LAST value survives, so there is
        // no "two runtimes were given" signal left to check downstream).
        if native_alias_has_user_supplied_runtime(&argv) {
            eprintln!(
                "zirv native: always selects the native runtime; drop --runtime or use `zirv \
                 chat --runtime <x>`"
            );
            std::process::exit(2);
        }
        // Issue #540: deliberately NO `maybe_run_first_run_wizard` call here
        // (unlike the `chat` alias above) -- native prerequisites are
        // diagnosed by `zirv ctx doctor`, never the wrapped-harness first-run
        // wizard, and this alias must not silently select a legacy runtime
        // either, so nothing here decides one: the rewritten argv always
        // names `--runtime native` explicitly.
        //
        // `NATIVE_ALIAS_ENV` is set on the process environment rather than
        // threaded through the rewritten argv (which would need a new hidden
        // flag on `ChatArgs`, visible to `zirv chat`'s own `--help` and
        // `zirv commands --json`): `run_native_chat` reads it back through
        // the same `EnvLookup` closure every other environment signal in
        // that function already goes through, so it can print the one-time
        // experimental banner only for this alias spelling, never for an
        // explicit `zirv chat --runtime native`, even though both launch
        // through the exact same function. `run_native_chat` clears it again
        // immediately after that one read, so no child process this session
        // later spawns (`wrap.rs`'s harness PTY, `dash/pane.rs`'s worker
        // panes -- neither clears the environment before spawning) inherits
        // it.
        //
        // SAFETY: `#[tokio::main]`'s runtime worker threads already exist at
        // this point, but nothing has been scheduled onto them yet --
        // `ctx::dispatch` below is the first call that does real work, and
        // it has not run yet -- so no other thread in this process can be
        // reading or writing the environment concurrently here.
        unsafe {
            std::env::set_var(ctx::chat::NATIVE_ALIAS_ENV, "true");
        }
        std::process::exit(ctx::dispatch(&rewrite_native_alias_args(&argv)));
    }

    if argv.len() == 1 {
        let stdin_is_tty = std::io::stdin().is_terminal();
        let stdout_is_tty = std::io::stdout().is_terminal();
        maybe_run_first_run_wizard(stdin_is_tty, stdout_is_tty, false);
        // Re-checked *after* the wizard: it may have just created a local
        // `.zirv` in this directory (step 6), which should immediately count
        // toward the target this same bare invocation resolves to.
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let zirv_exists = zirv_dir_present(&cwd);
        match bare_invocation_target(zirv_exists, stdin_is_tty, stdout_is_tty) {
            BareTarget::Chat => {
                std::process::exit(ctx::dispatch(&["ctx".to_string(), "chat".to_string()]));
            }
            BareTarget::Help => {
                if let Err(e) = show_help(&mut std::io::stdout(), console::colors_enabled()) {
                    output::error(e);
                    std::process::exit(1);
                }
                return;
            }
        }
    }

    if is_top_level_help(&argv) {
        if let Err(e) = show_help(&mut std::io::stdout(), console::colors_enabled()) {
            output::error(e);
            std::process::exit(1);
        }
        return;
    }

    let input = Input::parse();

    // These live on the shared `Input` struct, so clap accepts them for every
    // command and quietly eats the argument after them: `zirv deploy --name
    // staging` handed `staging` to `--name` and then complained the script got
    // no parameters. Refuse instead of swallowing.
    if !matches!(input.command.as_str(), "create" | "c")
        && let Some(flag) = input.misplaced_create_flag()
    {
        output::error(format!(
            "{flag} belongs to `zirv create`; '{}' does not take it",
            input.command
        ));
        std::process::exit(1);
    }

    match input.command.as_str() {
        "help" | "h" => {
            if let Err(e) = show_help(&mut std::io::stdout(), console::colors_enabled()) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "version" | "v" => {
            if let Err(e) = get_version(&mut std::io::stdout()) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "init" | "i" => {
            if let Err(e) = init_zirv(input.yes) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        "create" | "c" => {
            let opts = CreateOptions {
                name: input.name.clone(),
                shortcut: input.shortcut.clone(),
                global: input.global,
            };
            if let Err(e) = create_script(opts) {
                output::error(e);
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }

    // A command that case-folds to a reserved built-in but did not match one of
    // the exact-case arms above (e.g. `zirv Help`, `zirv CREATE`) must never
    // fall through to script lookup: on NTFS/APFS a repo `.zirv/Help.yaml`
    // resolves the same as `help.yaml` and would otherwise execute under a
    // reserved name that `zirv help` reports as shadowed and unreachable. The
    // pre-clap `ctx`/`chat`/`agent` interceptions already fold case; this closes
    // the same gap for the clap-dispatched built-ins.
    if utils::is_reserved_command(&input.command) {
        output::error(format!(
            "'{}' is a reserved command name and cannot be run as a script; \
             use the lowercase built-in",
            input.command
        ));
        std::process::exit(1);
    }

    let file_path = match input.get_file_path() {
        Ok(p) => p,
        Err(e) => {
            output::error(e);
            std::process::exit(1);
        }
    };

    let script = match file_to_script(&file_path) {
        Ok(s) => s,
        Err(e) => {
            output::error(e);
            std::process::exit(1);
        }
    };

    // Issue #330: a script `agent:` step drives a supervised worker session in
    // THIS process (see `script_runner::agent_command::run_supervised`), so
    // the harness it spawns -- and every cargo run under that harness --
    // inherits this process's scheduling class. Take the worker posture
    // before the script runs, and only for a script that actually delegates:
    // an ordinary shell script is the operator's own foreground work. A dry
    // run spawns nothing at all, so it is left alone. This is the one seam on
    // the script path no unit test drives; `run_supervised` itself is
    // exercised in-process by the step's own tests.
    if !input.dry_run && script.has_agent_step() {
        ctx::priority::apply_process(ctx::priority::posture_for(ctx::prompt::PromptRole::Worker));
    }

    if let Err(e) = execute(&script, &input.params, input.dry_run).await {
        output::error(&e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_is_top_level_help_matches_long_flag() {
        assert!(is_top_level_help(&argv(&["zirv", "--help"])));
    }

    #[test]
    fn test_is_top_level_help_matches_short_flag() {
        assert!(is_top_level_help(&argv(&["zirv", "-h"])));
    }

    #[test]
    fn test_is_top_level_help_ignores_script_commands() {
        assert!(!is_top_level_help(&argv(&["zirv", "build"])));
        assert!(!is_top_level_help(&argv(&["zirv", "help"])));
        assert!(!is_top_level_help(&argv(&["zirv"])));
    }

    #[test]
    fn test_is_top_level_help_does_not_match_flag_as_script_param() {
        // `--help` passed as a parameter to a script command (not in the
        // command slot itself) is not top-level help.
        assert!(!is_top_level_help(&argv(&["zirv", "build", "--help"])));
    }

    /// FINDING 2: the pre-clap `ctx`/`chat`/`agent` interceptions are matched
    /// case-insensitively (like `utils::is_reserved_command`), so a mis-cased
    /// invocation is routed to the built-in and can never fall through to a
    /// same-named repo script (`.zirv/Chat.yaml`, `Ctx.yaml`, ...).
    #[test]
    fn reserved_verbs_are_intercepted_case_insensitively() {
        assert!(is_top_level_ctx(&argv(&["zirv", "ctx", "status"])));
        assert!(is_top_level_ctx(&argv(&["zirv", "CTX", "status"])));
        assert!(is_top_level_ctx(&argv(&["zirv", "Ctx"])));
        assert!(!is_top_level_ctx(&argv(&["zirv", "context"])));
        assert!(!is_top_level_ctx(&argv(&["zirv"])));

        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "chat"])), Some("chat"));
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "Chat"])), Some("chat"));
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "CHAT"])), Some("chat"));
        assert_eq!(
            top_level_ctx_alias(&argv(&["zirv", "Agent"])),
            Some("agent")
        );
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "build"])), None);

        assert!(is_top_level_workflow_command(&argv(&["zirv", "skill"])));
        assert!(is_top_level_workflow_command(&argv(&["zirv", "SKILL"])));
        assert!(is_top_level_workflow_command(&argv(&["zirv", "frontend"])));
        assert!(is_top_level_workflow_command(&argv(&["zirv", "FRONTEND"])));
        assert!(!is_top_level_workflow_command(&argv(&["zirv", "build"])));

        assert!(is_top_level_setup(&argv(&["zirv", "setup"])));
        assert!(is_top_level_setup(&argv(&["zirv", "SETUP", "status"])));
        assert!(!is_top_level_setup(&argv(&["zirv", "setups"])));

        assert!(is_top_level_report(&argv(&[
            "zirv", "report", "bug", "title"
        ])));
        assert!(is_top_level_report(&argv(&[
            "zirv", "REPORT", "feature", "title"
        ])));
        assert!(!is_top_level_report(&argv(&["zirv", "reports"])));
    }

    /// Issue #540: `zirv native` (any casing) is intercepted as a top-level
    /// alias before clap ever runs, exactly like `chat`/`agent` above, and is
    /// reserved so a same-named script/shortcut can never shadow it.
    #[test]
    fn native_is_intercepted_case_insensitively_and_reserved() {
        assert!(is_top_level_native_alias(&argv(&["zirv", "native"])));
        assert!(is_top_level_native_alias(&argv(&["zirv", "Native"])));
        assert!(is_top_level_native_alias(&argv(&[
            "zirv", "NATIVE", "--foo"
        ])));
        assert!(!is_top_level_native_alias(&argv(&["zirv", "natives"])));
        assert!(!is_top_level_native_alias(&argv(&["zirv"])));
        assert!(utils::is_reserved_command("native"));
        assert!(utils::is_reserved_command("Native"));
    }

    /// The argv rewrite forwards every trailing argument byte-for-byte after
    /// splicing in `--runtime native`, so a native-compatible `zirv chat`
    /// option (or the existing native validator's exact refusal for a
    /// wrapped-only one) reaches `chat.rs` exactly as it would for `zirv chat
    /// --runtime native` directly.
    #[test]
    fn native_alias_rewrites_argv_to_chat_runtime_native() {
        assert_eq!(
            rewrite_native_alias_args(&argv(&["zirv", "native"])),
            vec![
                "ctx".to_string(),
                "chat".to_string(),
                "--runtime".to_string(),
                "native".to_string(),
            ]
        );
        assert_eq!(
            rewrite_native_alias_args(&argv(&["zirv", "native", "--foo"])),
            vec![
                "ctx".to_string(),
                "chat".to_string(),
                "--runtime".to_string(),
                "native".to_string(),
                "--foo".to_string(),
            ]
        );
        // Case-insensitive: the rewrite itself does not re-check argv[1]'s
        // casing (its caller, `is_top_level_native_alias`, already did), so
        // a mis-cased `NATIVE` still rewrites to the canonical lowercase
        // `--runtime native` value, the same way `rewrite_ctx_alias_args`
        // never re-derives the alias's own name from argv either.
        assert_eq!(
            rewrite_native_alias_args(&argv(&["zirv", "NATIVE", "--force-pace"])),
            vec![
                "ctx".to_string(),
                "chat".to_string(),
                "--runtime".to_string(),
                "native".to_string(),
                "--force-pace".to_string(),
            ]
        );
    }

    /// `zirv native --help`/`-h` is detected anywhere in the trailing argv
    /// (matching clap's own "--help short-circuits everything" convention),
    /// up to but not past a `--` separator -- text after that belongs to the
    /// adapter, not this alias.
    #[test]
    fn native_help_is_detected_anywhere_before_a_double_dash() {
        assert!(is_top_level_native_help(&argv(&[
            "zirv", "native", "--help"
        ])));
        assert!(is_top_level_native_help(&argv(&["zirv", "native", "-h"])));
        assert!(is_top_level_native_help(&argv(&[
            "zirv", "NATIVE", "--help"
        ])));
        // Review finding: `--help` after some other flag, or with a
        // trailing argument after it, must still be detected -- only a
        // literal `--` boundary opts a later `--help` out.
        assert!(is_top_level_native_help(&argv(&[
            "zirv", "native", "--foo", "--help"
        ])));
        assert!(is_top_level_native_help(&argv(&[
            "zirv", "native", "--help", "extra"
        ])));
        assert!(!is_top_level_native_help(&argv(&["zirv", "native"])));
        assert!(!is_top_level_native_help(&argv(&[
            "zirv", "native", "--foo"
        ])));
        // `--help` past a `--` is an opaque extra argument meant for the
        // adapter, not a request for this alias's own help text.
        assert!(!is_top_level_native_help(&argv(&[
            "zirv", "native", "--", "--help"
        ])));
    }

    /// Review finding (issue #540): `zirv native --runtime harness` used to
    /// rewrite to `ctx chat --runtime native --runtime harness`, and clap's
    /// `Option<String>` let the LATER `--runtime` win, so the alias silently
    /// launched the wrapped harness. Both spellings (`--runtime harness` and
    /// `--runtime=harness`) must be caught, case-insensitively on `native`,
    /// and a `--runtime` past a `--` separator (opaque extra argv for the
    /// adapter) must not trip the guard.
    #[test]
    fn native_alias_refuses_a_user_supplied_runtime() {
        assert!(native_alias_has_user_supplied_runtime(&argv(&[
            "zirv",
            "native",
            "--runtime",
            "harness"
        ])));
        assert!(native_alias_has_user_supplied_runtime(&argv(&[
            "zirv",
            "native",
            "--runtime=harness"
        ])));
        assert!(native_alias_has_user_supplied_runtime(&argv(&[
            "zirv",
            "NATIVE",
            "--force-pace",
            "--runtime",
            "native"
        ])));
        assert!(!native_alias_has_user_supplied_runtime(&argv(&[
            "zirv", "native"
        ])));
        assert!(!native_alias_has_user_supplied_runtime(&argv(&[
            "zirv",
            "native",
            "--force-pace"
        ])));
        assert!(!native_alias_has_user_supplied_runtime(&argv(&[
            "zirv",
            "native",
            "--",
            "--runtime",
            "harness"
        ])));
        // Not the `native` alias at all -- must not false-positive on an
        // ordinary `zirv chat --runtime harness`.
        assert!(!native_alias_has_user_supplied_runtime(&argv(&[
            "zirv",
            "chat",
            "--runtime",
            "harness"
        ])));
    }

    /// Exercised through the real built binary (the same pattern
    /// `memory_is_intercepted_before_script_lookup` below uses): the
    /// interception happens on raw argv before `Input::parse()`, which a
    /// purely in-process call to `main`'s own helpers cannot prove end to
    /// end, including the process exit code.
    #[test]
    fn native_help_exits_0_and_prints_the_experimental_notice() {
        let exe = std::env::current_exe().expect("current_exe");
        let bin = exe
            .parent()
            .and_then(|p| p.parent())
            .expect("target/debug")
            .join("zirv");

        let out = std::process::Command::new(&bin)
            .args(["native", "--help"])
            .output()
            .expect("run zirv");

        assert!(
            out.status.success(),
            "exit code: {:?}, stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("EXPERIMENTAL / WORK IN PROGRESS"),
            "got: {text}"
        );
        assert!(text.contains("zirv chat --runtime native"), "got: {text}");
    }

    #[test]
    fn update_is_intercepted_case_insensitively() {
        assert!(is_top_level_update(&argv(&["zirv", "update"])));
        assert!(is_top_level_update(&argv(&[
            "zirv",
            "UPDATE",
            "--version",
            "3.20.0"
        ])));
        assert!(!is_top_level_update(&argv(&["zirv", "updates"])));
    }

    /// FINDING 2: every reserved command name -- whatever its casing -- is
    /// recognised by the guard that gates script dispatch, so `zirv Help`,
    /// `zirv CREATE`, `zirv Chat` are all refused as scripts rather than run a
    /// repo file that case-folds to a built-in.
    #[test]
    fn mis_cased_reserved_command_names_are_recognised_by_the_guard() {
        for name in [
            "Help", "HELP", "Version", "CREATE", "Init", "Ctx", "Chat", "Agent", "Setup", "Report",
            "Update",
        ] {
            assert!(
                utils::is_reserved_command(name),
                "'{name}' must be recognised as reserved so it can never run as a script"
            );
        }
        assert!(!utils::is_reserved_command("build"));
        assert!(!utils::is_reserved_command("deploy"));
    }

    #[test]
    fn a_bare_invocation_in_a_repo_with_a_zirv_directory_starts_chat() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(cwd.path().join(".zirv")).expect("mkdir");

        assert!(zirv_dir_present(cwd.path()));
        assert_eq!(bare_invocation_target(true, true, true), BareTarget::Chat);
    }

    /// S5: a *global* `~/.zirv` alone must not turn a bare `zirv` into a chat
    /// launch in an arbitrary directory -- only a local `./.zirv` counts.
    #[test]
    fn a_bare_invocation_with_only_a_global_zirv_directory_shows_help() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");

        assert!(
            !zirv_dir_present(cwd.path()),
            "a global .zirv is not a local one"
        );
        assert_eq!(bare_invocation_target(false, true, true), BareTarget::Help);
    }

    #[test]
    fn a_bare_invocation_with_no_zirv_directory_anywhere_shows_help() {
        let cwd = tempfile::tempdir().expect("tempdir");

        assert!(!zirv_dir_present(cwd.path()));
        assert_eq!(bare_invocation_target(false, true, true), BareTarget::Help);
    }

    #[test]
    fn a_bare_invocation_with_piped_stdin_shows_help_instead_of_opening_a_chat() {
        // A `.zirv` directory exists, but stdin is not a real terminal (e.g.
        // `echo hi | zirv`): help wins, so a bare invocation piped into a
        // script never blocks on an interactive chat session.
        assert_eq!(bare_invocation_target(true, false, true), BareTarget::Help);
    }

    /// S5: stdin alone being a terminal is not enough -- `zirv | less` has an
    /// interactive stdin but a redirected stdout, and must not launch a chat
    /// session into the pipe on the other end.
    #[test]
    fn a_bare_invocation_with_piped_stdout_shows_help_instead_of_opening_a_chat_into_the_pipe() {
        assert_eq!(bare_invocation_target(true, true, false), BareTarget::Help);
    }

    #[test]
    fn an_explicit_chat_command_is_not_subject_to_the_tty_rule() {
        // `zirv chat` is routed by `top_level_ctx_alias`, an entirely
        // different code path from the bare-invocation TTY check: it takes
        // no stdin-is-a-terminal parameter at all, so a piped `zirv chat`
        // still launches chat (and lets `wrap`/the adapter itself decide
        // what to do with a non-interactive terminal).
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "chat"])), Some("chat"));
        assert_eq!(
            rewrite_ctx_alias_args("chat", &argv(&["zirv", "chat"])),
            vec!["ctx".to_string(), "chat".to_string()]
        );
        assert_eq!(
            rewrite_ctx_alias_args("agent", &argv(&["zirv", "agent", "claude", "go"])),
            vec![
                "ctx".to_string(),
                "agent".to_string(),
                "claude".to_string(),
                "go".to_string()
            ]
        );
    }

    /// Issue #267: `--mode`/`--worktree` are ordinary trailing argv to this
    /// alias rewrite -- it forwards everything after the verb byte-for-byte,
    /// so a new `AgentArgs` flag needs no rewrite-side change to reach
    /// `zirv ctx agent`'s own clap parser.
    #[test]
    fn the_agent_alias_forwards_mode_and_worktree_flags_unchanged() {
        assert_eq!(
            rewrite_ctx_alias_args(
                "agent",
                &argv(&[
                    "zirv",
                    "agent",
                    "claude",
                    "go",
                    "--mode",
                    "read-only",
                    "--worktree"
                ])
            ),
            vec![
                "ctx".to_string(),
                "agent".to_string(),
                "claude".to_string(),
                "go".to_string(),
                "--mode".to_string(),
                "read-only".to_string(),
                "--worktree".to_string(),
            ]
        );
    }

    #[test]
    fn the_help_flag_still_wins_over_the_bare_alias() {
        // `zirv --help` has argv len 2, not 1, so it never reaches the bare-
        // invocation check at all, and it is not a `chat`/`agent` alias
        // either.
        assert!(is_top_level_help(&argv(&["zirv", "--help"])));
        assert_eq!(top_level_ctx_alias(&argv(&["zirv", "--help"])), None);
    }

    #[test]
    fn chat_and_agent_are_reserved_so_a_script_can_never_shadow_them() {
        assert!(utils::is_reserved_command("chat"));
        assert!(utils::is_reserved_command("agent"));
    }

    /// Issue #355: the top-level `--skill` flag and a bare `zirv skill` (no
    /// further argument) both request the bundled operator skill, with or
    /// without `--json`. Case-insensitive for the bare-word form, matching
    /// every other reserved built-in.
    #[test]
    fn top_level_skill_request_matches_the_flag_and_bare_word_forms() {
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "--skill"])),
            Some(false)
        );
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "--skill", "--json"])),
            Some(true)
        );
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "skill"])),
            Some(false)
        );
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "SKILL"])),
            Some(false)
        );
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "skill", "--json"])),
            Some(true)
        );
        assert_eq!(top_level_skill_request(&argv(&["zirv", "build"])), None);
    }

    /// `zirv skill list`/`zirv skill show <id>` are the pre-existing
    /// `workflow::skill` engineering-skill tree, not the bundled operator
    /// skill: a trailing argument other than `--json` must fall through to
    /// `is_top_level_workflow_command`'s own dispatch instead of being
    /// treated as this built-in.
    #[test]
    fn top_level_skill_request_leaves_the_engineering_skill_subcommands_alone() {
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "skill", "list"])),
            None
        );
        assert_eq!(
            top_level_skill_request(&argv(&["zirv", "skill", "show", "delegate"])),
            None
        );
        assert!(is_top_level_workflow_command(&argv(&[
            "zirv", "skill", "list"
        ])));
    }

    #[test]
    fn top_level_commands_request_matches_with_and_without_json() {
        assert_eq!(
            top_level_commands_request(&argv(&["zirv", "commands"])),
            Some(false)
        );
        assert_eq!(
            top_level_commands_request(&argv(&["zirv", "COMMANDS", "--json"])),
            Some(true)
        );
        assert_eq!(top_level_commands_request(&argv(&["zirv", "build"])), None);
        assert!(utils::is_reserved_command("commands"));
    }

    #[test]
    fn memory_is_intercepted_case_insensitively_and_reserved() {
        assert!(is_top_level_memory(&argv(&["zirv", "memory", "status"])));
        assert!(is_top_level_memory(&argv(&["zirv", "Memory"])));
        assert!(is_top_level_memory(&argv(&["zirv", "MEMORY", "list"])));
        assert!(!is_top_level_memory(&argv(&["zirv", "memories"])));
        assert!(!is_top_level_memory(&argv(&["zirv"])));
        assert!(utils::is_reserved_command("memory"));
        assert!(utils::is_reserved_command("Memory"));
    }

    #[test]
    fn first_run_wizard_runs_only_when_interactive_and_unconfigured() {
        assert!(first_run_wizard_should_run(true, true, true));
        assert!(!first_run_wizard_should_run(false, true, true));
        assert!(!first_run_wizard_should_run(true, false, true));
        assert!(
            !first_run_wizard_should_run(true, true, false),
            "already configured -- must not re-run"
        );
    }

    /// A repo `.zirv/Memory.yaml` (any casing) must never be reachable as a
    /// script -- `memory` is intercepted against raw argv before clap runs,
    /// the same guarantee `ctx_is_intercepted_before_script_lookup` pins for
    /// `ctx`.
    #[test]
    fn memory_is_intercepted_before_script_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            dir.path().join(".zirv/Memory.yaml"),
            "commands:\n  - command: echo not-a-built-in\n",
        )
        .expect("write");

        let exe = std::env::current_exe().expect("current_exe");
        let bin = exe
            .parent()
            .and_then(|p| p.parent())
            .expect("target/debug")
            .join("zirv");

        let out = std::process::Command::new(&bin)
            .args(["memory", "--help"])
            .current_dir(dir.path())
            .output()
            .expect("run zirv");

        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("status") && text.contains("remember"),
            "built-in memory help expected, got: {text}"
        );
    }

    /// Issue #45: `context` is a new top-level built-in, sibling to `ctx`
    /// rather than a verb under it (see `commands::ctx::context_cli`'s
    /// module doc). It gets its own interception, matched case-insensitively
    /// like every other one, and is reserved so a script can never shadow it.
    #[test]
    fn context_is_intercepted_case_insensitively_and_reserved() {
        assert!(is_top_level_context(&argv(&["zirv", "context", "sync"])));
        assert!(is_top_level_context(&argv(&["zirv", "CONTEXT", "sync"])));
        assert!(is_top_level_context(&argv(&["zirv", "Context"])));
        assert!(!is_top_level_context(&argv(&["zirv", "ctx"])));
        assert!(!is_top_level_context(&argv(&["zirv"])));

        assert!(utils::is_reserved_command("context"));
        assert!(utils::is_reserved_command("Context"));
    }

    /// `.zirv/context/` is a *directory* holding canonical instructions
    /// (`commands::ctx::context::CONTEXT_DIR`) that already exists before
    /// this feature; a script file named `context.yaml` sitting next to it
    /// is a completely different thing and must be reported as shadowed
    /// (unreachable, since the built-in wins), never silently confused with
    /// the directory or allowed to run instead of the built-in.
    #[test]
    fn a_context_directory_and_a_shadowed_context_script_coexist_in_listing()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path = temp_dir.path().to_path_buf();
        let zirv_dir = temp_path.join(".zirv");
        let commands_dir = zirv_dir.join(utils::COMMANDS_DIR_NAME);
        std::fs::create_dir_all(zirv_dir.join("context"))?;
        std::fs::write(
            zirv_dir.join("context").join("common.md"),
            "Always run tests.",
        )?;
        std::fs::create_dir_all(&commands_dir)?;
        std::fs::write(
            commands_dir.join("context.yaml"),
            "name: \"My Context Script\"\ncommands: []\n",
        )?;
        std::fs::write(
            commands_dir.join("build.yaml"),
            "name: \"Build\"\ncommands: []\n",
        )?;

        let _cwd = commands::ctx::testenv::CwdGuard::enter(&temp_path)?;
        let mut buffer = std::io::Cursor::new(Vec::new());
        commands::help::show_help(&mut buffer, false)?;
        let output = String::from_utf8(buffer.into_inner())?;

        // The directory itself must never be listed as an invocable script.
        assert!(
            !output.contains("File: context\n") && !output.contains("File: context "),
            "the context/ directory must not appear as a script entry: {output}"
        );
        let context_line = output
            .lines()
            .find(|l| l.starts_with("File: context.yaml"))
            .unwrap_or("");
        assert!(
            context_line.contains("shadowed") || context_line.contains("unreachable"),
            "expected 'context.yaml' to be marked unreachable, got: {context_line}"
        );
        let build_line = output
            .lines()
            .find(|l| l.starts_with("File: build.yaml"))
            .unwrap_or("");
        assert!(
            !build_line.contains("shadowed"),
            "an ordinary script must not be marked shadowed, got: {build_line}"
        );
        Ok(())
    }
}
