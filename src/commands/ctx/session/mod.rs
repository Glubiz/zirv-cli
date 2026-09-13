//! `zirv session` (issue #352): the persistent runtime -- a local service that
//! OWNS the pty/ConPTY processes, so a session survives the client that was
//! looking at it.
//!
//! Layout mirrors `api/`, deliberately: [`namespace`] is the durable identity
//! record, [`host`] is the terminals, [`service`] is the process that binds
//! the endpoint and serves them over protocol v1, and [`client`] is every
//! surface that talks to one. Nothing here invents a second protocol: the
//! dashboard, the CLI and any alternate client all go through `api::wire`'s
//! `session.attach|detach|takeover|resize|screen` and the ordinary
//! `session.start|stop|send_input`.
//!
//! Everything is gated on `[session] persistent` (operator-only, off by
//! default -- see `config::SessionConfig`). With the gate off, `zirv session
//! serve` refuses to start and every other surface behaves exactly as it did
//! before the feature existed.
//!
//! The verbs and what separates them:
//!
//! | verb | what it touches | what it never touches |
//! |---|---|---|
//! | `serve` | binds the endpoint, owns the terminals | -- |
//! | `list` | reads | -- |
//! | `attach` | this client's attachment | the child process |
//! | `detach` | this client's attachment | the child process |
//! | `stop` | the child process, through the existing ladder | -- |
//!
//! `detach` and `stop` are separate verbs with separate confirmation and exit
//! behaviour precisely because conflating them is the failure mode issue #352
//! exists to remove: closing a window must never kill an agent.

pub mod client;
pub mod host;
pub mod namespace;
pub mod service;

use std::io::{IsTerminal, Write};

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use super::CtxResult;
use super::api::server::endpoint_for;
use super::api::wire::{Method, SessionState};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::state::StateDir;

/// The default bound lifetime of `zirv session serve` when the operator gives
/// no `--seconds`: none. A runtime that timed out would be a worse promise
/// than no runtime at all -- the operator stops it explicitly with `zirv
/// session stop --runtime`, which is the same distinction `detach` and `stop`
/// draw one level down.
#[derive(Debug, Parser)]
#[command(
    name = "zirv session",
    about = "Own, attach to and stop persistent zirv runtime sessions.",
    disable_help_subcommand = true
)]
pub struct SessionCli {
    #[command(subcommand)]
    pub verb: SessionVerb,
}

#[derive(Debug, Subcommand)]
pub enum SessionVerb {
    /// Run the persistent runtime service in this process: own the terminals,
    /// serve protocol v1, and keep every session alive until it is stopped.
    Serve(ServeArgs),
    /// List the sessions the runtime owns, with their attachment state.
    List(ListArgs),
    /// Attach this terminal to a runtime session.
    Attach(AttachArgs),
    /// Release a session's clients without touching the agent running in it.
    Detach(DetachArgs),
    /// Stop a session (or the whole runtime). This is the verb that ends
    /// processes.
    Stop(StopArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The runtime namespace to publish. One namespace per state directory:
    /// the endpoint is derived from the state directory, so a second
    /// namespace needs its own `ZIRV_CTX_STATE_DIR`.
    #[arg(long, default_value = namespace::DEFAULT_NAMESPACE)]
    pub namespace: String,
    /// Stop serving after this many seconds instead of running until asked to
    /// stop. Bounded runs are what the tests and `zirv chat`'s own autostart
    /// use; an operator normally wants the unbounded form.
    #[arg(long)]
    pub seconds: Option<u64>,
    /// Do not restore sessions from the stored topology on startup.
    #[arg(long, default_value_t = false)]
    pub no_restore: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long, default_value = namespace::DEFAULT_NAMESPACE)]
    pub namespace: String,
    /// Emit machine-readable JSON.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    /// Session name or id. With none, the only live session.
    pub target: Option<String>,
    /// Watch without taking the keyboard. Any number of observers may attach.
    #[arg(long, default_value_t = false)]
    pub observer: bool,
    /// Take the controller seat even though another client holds it. Visible
    /// to every other client as a `controller_changed` event.
    #[arg(long, default_value_t = false)]
    pub takeover: bool,
}

#[derive(Debug, Args)]
pub struct DetachArgs {
    /// Session name or id. With none, the only live session.
    pub target: Option<String>,
    /// Detach every client, not just the controller. Useful after a client
    /// crashed without saying goodbye.
    #[arg(long, default_value_t = false)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    /// Session name or id. With none, the only live session.
    pub target: Option<String>,
    /// Answer the confirmation. Required when stdin is not a terminal.
    #[arg(long, default_value_t = false)]
    pub yes: bool,
    /// Stop the runtime SERVICE rather than a session. Sessions keep running
    /// unless `--stop-sessions` is given too.
    #[arg(long, default_value_t = false)]
    pub runtime: bool,
    /// With `--runtime`: also put every session through the termination
    /// ladder on the way out.
    #[arg(long, default_value_t = false)]
    pub stop_sessions: bool,
    #[arg(long, default_value = namespace::DEFAULT_NAMESPACE)]
    pub namespace: String,
}

/// The refusal every verb shares when the operator has not opted in. Named
/// once so the four surfaces cannot word the same gate four ways.
pub const GATE_OFF: &str = "the persistent runtime is experimental and off by default: set \
     `[session] persistent = true` in ~/.zirv/ctx.toml (or \
     ZIRV_CTX_SESSION_PERSISTENT=true). A repository cannot turn it on.";

pub fn dispatch(args: &[String]) -> i32 {
    let argv = std::iter::once("zirv session".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match SessionCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return match err.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            };
        }
    };
    let mut out = std::io::stdout();
    match run(&cli, &mut out, &env_from_process()) {
        Ok(code) => code,
        Err(error) => {
            crate::output::error(error);
            1
        }
    }
}

pub fn run<W: Write>(cli: &SessionCli, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, env)?;
    if !cfg.session.persistent {
        writeln!(w, "{GATE_OFF}")?;
        return Ok(1);
    }
    let state = StateDir::resolve(env)?;
    match &cli.verb {
        SessionVerb::Serve(args) => run_serve(args, &cfg, state, w),
        SessionVerb::List(args) => run_list(args, &state, w),
        SessionVerb::Attach(args) => run_attach(args, &state, w),
        SessionVerb::Detach(args) => run_detach(args, &state, w),
        SessionVerb::Stop(args) => run_stop(args, &state, w),
    }
}

fn run_serve<W: Write>(
    args: &ServeArgs,
    cfg: &CtxConfig,
    state: StateDir,
    w: &mut W,
) -> CtxResult<i32> {
    let service = service::RuntimeService::start(state, &args.namespace, cfg)?;
    writeln!(
        w,
        "zirv runtime '{}' serving protocol v{} on {} (instance {})",
        args.namespace,
        super::api::wire::PROTOCOL_VERSION,
        service.endpoint().display(),
        service.instance()
    )?;
    if let Some(warning) = service.host().history_warning() {
        writeln!(w, "{warning}")?;
    }
    if !args.no_restore {
        let report = service.restore(cfg);
        for id in &report.resumed {
            writeln!(w, "  resumed {id} from its stored conversation")?;
        }
        for entry in &report.skipped {
            writeln!(
                w,
                "  not resumed: {} ({}) had no verified conversation reference -- its process is \
                 gone, and zirv does not pretend otherwise",
                entry.short, entry.agent
            )?;
        }
        for (short, error) in &report.failed {
            writeln!(w, "  could not resume {short}: {error}")?;
        }
    }
    w.flush()?;
    let until = args
        .seconds
        .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs(secs));
    let code = service.serve(w, until)?;
    // `stop_sessions = false`: the service exits, the agents do not. Only
    // `zirv session stop --runtime --stop-sessions` says otherwise.
    service.shutdown(false);
    writeln!(w, "zirv runtime '{}' stopped; sessions left running", args.namespace)?;
    Ok(code)
}

fn run_list<W: Write>(args: &ListArgs, state: &StateDir, w: &mut W) -> CtxResult<i32> {
    let record = namespace::read(state, &args.namespace);
    let endpoint = endpoint_for(state);
    let mut client = match client::connect(&endpoint) {
        Ok(client) => client,
        Err(error) => {
            if args.json {
                writeln!(w, "{}", json!({ "runtime": null, "sessions": [] }))?;
            } else {
                writeln!(w, "{error}")?;
            }
            return Ok(1);
        }
    };
    let sessions = client::snapshot(&mut client)?;
    if args.json {
        writeln!(
            w,
            "{}",
            serde_json::to_string_pretty(&json!({
                "runtime": record,
                "attachable": client::can_attach(&client),
                "sessions": sessions,
            }))?
        )?;
        return Ok(0);
    }
    match &record {
        Some(record) => writeln!(
            w,
            "runtime '{}' v{} (pid {}, protocol {}, history {}), up since {}",
            record.name,
            record.version,
            record.owner.pid,
            record.protocol,
            if record.history { "on" } else { "off" },
            record.created_at
        )?,
        None => {
            let known = namespace::list(state);
            writeln!(
                w,
                "no runtime record for '{}'{}",
                args.namespace,
                if known.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (known: {})",
                        known
                            .iter()
                            .map(|record| record.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            )?;
        }
    }
    if !client::can_attach(&client) {
        writeln!(w, "{}", client::NO_TERMINALS)?;
    }
    if sessions.is_empty() {
        writeln!(w, "  (no sessions)")?;
    }
    for facts in &sessions {
        writeln!(
            w,
            "  {}  {:<8}  {:<10}  {}",
            facts.short,
            facts.agent.as_deref().unwrap_or("-"),
            format!("{:?}", facts.state).to_lowercase(),
            facts.role.as_deref().unwrap_or("-")
        )?;
    }
    Ok(0)
}

fn run_attach<W: Write>(args: &AttachArgs, state: &StateDir, w: &mut W) -> CtxResult<i32> {
    let endpoint = endpoint_for(state);
    let mut client = client::connect(&endpoint)?;
    if !client::can_attach(&client) {
        writeln!(w, "{}", client::NO_TERMINALS)?;
        return Ok(1);
    }
    let sessions = client::snapshot(&mut client)?;
    let target = match client::resolve_target(&sessions, args.target.as_deref()) {
        Ok(facts) => facts.session_id.clone(),
        Err(error) => {
            writeln!(w, "{error}")?;
            return Ok(1);
        }
    };
    let name = client::client_id("attach");
    let outcome = client::attach_terminal(
        &mut client,
        &target,
        &name,
        !args.observer,
        args.takeover,
        w,
    )?;
    match outcome {
        client::AttachOutcome::Detached => writeln!(
            w,
            "detached from {target}; the session and its agent are still running \
             (`zirv session stop` ends one)"
        )?,
        client::AttachOutcome::SessionEnded => writeln!(w, "session {target} ended")?,
    }
    Ok(0)
}

fn run_detach<W: Write>(args: &DetachArgs, state: &StateDir, w: &mut W) -> CtxResult<i32> {
    let endpoint = endpoint_for(state);
    let mut client = client::connect(&endpoint)?;
    if !client::can_attach(&client) {
        writeln!(w, "{}", client::NO_TERMINALS)?;
        return Ok(1);
    }
    let sessions = client::snapshot(&mut client)?;
    let target = match client::resolve_target(&sessions, args.target.as_deref()) {
        Ok(facts) => facts.session_id.clone(),
        Err(error) => {
            writeln!(w, "{error}")?;
            return Ok(1);
        }
    };
    // No confirmation, on purpose: detaching cannot lose work. Stopping can,
    // which is why only that verb asks.
    let attachment = client.call(
        Method::SessionAttach,
        json!({ "session_id": target, "client_id": client::client_id("detach") }),
    )?;
    let clients: Vec<String> = attachment
        .get("attachment")
        .and_then(|value| value.get("clients"))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    let controller: Option<String> = attachment
        .get("attachment")
        .and_then(|value| value.get("controller"))
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let mut detached = 0usize;
    let mut targets: Vec<String> = if args.all {
        clients
    } else {
        controller.into_iter().collect()
    };
    targets.push(client::client_id("detach"));
    targets.sort();
    targets.dedup();
    for id in targets {
        if client
            .call(
                Method::SessionDetach,
                json!({ "session_id": target, "client_id": id }),
            )
            .is_ok()
        {
            detached += 1;
        }
    }
    writeln!(
        w,
        "detached {detached} client(s) from {target}; the agent is untouched"
    )?;
    Ok(0)
}

fn run_stop<W: Write>(args: &StopArgs, state: &StateDir, w: &mut W) -> CtxResult<i32> {
    if args.runtime {
        if !confirmed(args.yes, "stop the zirv runtime service", w)? {
            return Ok(2);
        }
        if args.stop_sessions {
            // Every session, through the ordinary `session.stop` ladder --
            // the operator asked for exactly that.
            let endpoint = endpoint_for(state);
            if let Ok(mut client) = client::connect(&endpoint) {
                for facts in client::snapshot(&mut client)?
                    .into_iter()
                    .filter(|facts| facts.state != SessionState::Ended)
                {
                    let _ = client.call(
                        Method::SessionStop,
                        json!({ "session_id": facts.session_id }),
                    );
                }
            }
        }
        service::request_shutdown(state, &args.namespace)?;
        writeln!(
            w,
            "asked runtime '{}' to stop{}",
            args.namespace,
            if args.stop_sessions {
                " and stopped its sessions"
            } else {
                "; its sessions keep running"
            }
        )?;
        return Ok(0);
    }

    let endpoint = endpoint_for(state);
    let mut client = client::connect(&endpoint)?;
    let sessions = client::snapshot(&mut client)?;
    let target = match client::resolve_target(&sessions, args.target.as_deref()) {
        Ok(facts) => facts.clone(),
        Err(error) => {
            writeln!(w, "{error}")?;
            return Ok(1);
        }
    };
    if target.state == SessionState::Ended {
        writeln!(w, "{} has already ended", target.short)?;
        return Ok(1);
    }
    if !confirmed(
        args.yes,
        &format!(
            "stop {} ({}) -- this terminates the agent",
            target.short,
            target.agent.as_deref().unwrap_or("session")
        ),
        w,
    )? {
        return Ok(2);
    }
    let result = client.call(
        Method::SessionStop,
        json!({ "session_id": target.session_id }),
    )?;
    let stopped = result
        .get("stopped")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if stopped {
        writeln!(w, "stopped {}", target.short)?;
        Ok(0)
    } else {
        writeln!(w, "{} was already stopped", target.short)?;
        Ok(1)
    }
}

// ---------------------------------------------------------------------------
// `zirv chat`'s route into the runtime
// ---------------------------------------------------------------------------

/// Where a `zirv chat` invocation's session is going to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRoute {
    /// The persistent runtime owns the pty; this process is only a client.
    Runtime,
    /// Today's behaviour exactly: this process owns the pty and the session
    /// ends with it.
    InProcess,
}

/// Pure: which route a chat launch takes. Three ways to stay on the old path,
/// and every one of them is deliberate -- the gate is off (the default), the
/// operator asked for the escape hatch, or there is no terminal to attach,
/// which is the case every script and CI job is in.
pub fn chat_route(
    persistent: bool,
    no_session: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> ChatRoute {
    if persistent && !no_session && stdin_is_tty && stdout_is_tty {
        ChatRoute::Runtime
    } else {
        ChatRoute::InProcess
    }
}

/// How long a freshly-spawned runtime gets to bind its endpoint before the
/// caller gives up and takes the in-process path instead.
const SERVICE_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Attaches this terminal to the default runtime, starting the runtime and/or
/// the session if they are not there yet. Returns the process exit code.
///
/// Every step is an ordinary protocol v1 call: `session.snapshot` to find an
/// existing seat for this repository, `session.start` to open one, then the
/// attachment surface. Nothing about this path is private to `zirv chat`.
pub fn chat_via_runtime<W: Write>(
    state: &StateDir,
    agent: &str,
    prompt: Option<&str>,
    extra: &[String],
    repo: &std::path::Path,
    w: &mut W,
) -> CtxResult<i32> {
    let endpoint = endpoint_for(state);
    if !super::api::transport::probe(&endpoint) {
        spawn_service(state)?;
        wait_for_endpoint(&endpoint, SERVICE_START_TIMEOUT)?;
    }
    let mut client = client::connect(&endpoint)?;
    if !client::can_attach(&client) {
        return Err(client::NO_TERMINALS.into());
    }
    let slug = super::state::repo_slug(repo);
    let existing = client::snapshot(&mut client)?.into_iter().find(|facts| {
        facts.state != SessionState::Ended
            && facts.repo_slug.as_deref() == Some(slug.as_str())
            && facts.agent.as_deref() == Some(agent)
    });
    let session_id = match existing {
        Some(facts) => {
            writeln!(w, "zirv chat: attaching to {} on the runtime", facts.short)?;
            facts.session_id
        }
        None => {
            let started = client.call(
                Method::SessionStart,
                json!({
                    "runtime": "harness",
                    "role": "orchestrator",
                    "agent": agent,
                    "cwd": repo.to_string_lossy(),
                    "prompt": prompt.unwrap_or_default(),
                    "extra_args": extra,
                }),
            )?;
            serde_json::from_value::<String>(started["session"]["session_id"].clone())?
        }
    };
    let name = client::client_id("chat");
    let outcome = client::attach_terminal(&mut client, &session_id, &name, true, false, w)?;
    match outcome {
        client::AttachOutcome::Detached => writeln!(
            w,
            "detached; the session keeps running -- `zirv session attach` comes back to it, \
             `zirv session stop` ends it"
        )?,
        client::AttachOutcome::SessionEnded => writeln!(w, "the session ended")?,
    }
    Ok(0)
}

/// Starts `zirv session serve` as a detached process. Detached in the same
/// two ways `workflow::engine` already detaches a background worker -- its own
/// process group off unix, `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on
/// Windows -- because a runtime that died with the terminal that happened to
/// start it would defeat the entire feature.
fn spawn_service(state: &StateDir) -> CtxResult<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("session")
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Explicit rather than inherited: the client resolved this state
        // directory, and the runtime it starts has to be the one it is about
        // to connect to.
        .env("ZIRV_CTX_STATE_DIR", state.root());
    detach(&mut command);
    command.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

fn wait_for_endpoint(
    endpoint: &super::api::transport::Endpoint,
    timeout: std::time::Duration,
) -> CtxResult<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if super::api::transport::probe(endpoint) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(format!(
        "the runtime did not start listening on {} within {timeout:?}",
        endpoint.display()
    )
    .into())
}

/// The one confirmation rule: an interactive operator is asked, a
/// non-interactive one must have said `--yes` in advance. Never assumes yes
/// from a pipe -- a scripted `zirv session stop` that silently killed an
/// agent would be the exact accident this feature is supposed to prevent.
fn confirmed<W: Write>(yes: bool, what: &str, w: &mut W) -> CtxResult<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        writeln!(w, "refusing to {what} without --yes (stdin is not a terminal)")?;
        return Ok(false);
    }
    let answer = dialoguer::Confirm::new()
        .with_prompt(format!("{what}?"))
        .default(false)
        .interact()
        .unwrap_or(false);
    if !answer {
        writeln!(w, "cancelled")?;
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn render(cli: &SessionCli, env: &HashMap<String, String>) -> (i32, String) {
        let mut out = Vec::new();
        let code = run(cli, &mut out, &|key| env.get(key).cloned()).expect("run");
        (code, String::from_utf8(out).expect("utf-8"))
    }

    #[test]
    fn every_verb_parses() {
        assert!(matches!(
            SessionCli::try_parse_from(["zirv session", "serve", "--seconds", "1"])
                .expect("serve")
                .verb,
            SessionVerb::Serve(_)
        ));
        assert!(matches!(
            SessionCli::try_parse_from(["zirv session", "list", "--json"])
                .expect("list")
                .verb,
            SessionVerb::List(_)
        ));
        assert!(matches!(
            SessionCli::try_parse_from(["zirv session", "attach", "abc", "--observer"])
                .expect("attach")
                .verb,
            SessionVerb::Attach(_)
        ));
        assert!(matches!(
            SessionCli::try_parse_from(["zirv session", "detach", "--all"])
                .expect("detach")
                .verb,
            SessionVerb::Detach(_)
        ));
        assert!(matches!(
            SessionCli::try_parse_from(["zirv session", "stop", "--runtime", "--yes"])
                .expect("stop")
                .verb,
            SessionVerb::Stop(_)
        ));
    }

    /// The gate, from the outside: with nothing configured, every verb says
    /// so and does nothing. This is what "staged behind an experimental
    /// operator-only flag" has to mean at the CLI boundary.
    #[test]
    fn every_verb_refuses_while_the_gate_is_off() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let env = env_with(&[(
            "ZIRV_CTX_STATE_DIR",
            &tmp.path().to_string_lossy().into_owned(),
        )]);
        for argv in [
            vec!["zirv session", "list"],
            vec!["zirv session", "attach"],
            vec!["zirv session", "detach"],
            vec!["zirv session", "stop", "--yes"],
        ] {
            let cli = SessionCli::try_parse_from(argv.clone()).expect("parse");
            let (code, text) = render(&cli, &env);
            assert_eq!(code, 1, "{argv:?}");
            assert!(text.contains("experimental"), "{argv:?}: {text}");
            assert!(text.contains("ZIRV_CTX_SESSION_PERSISTENT"), "{text}");
        }
    }

    /// `zirv chat` reaches the runtime only when the operator opted in AND
    /// there is a terminal to attach. Every other combination -- the gate off
    /// (the default), `--no-session`, a piped stdin, a redirected stdout --
    /// keeps today's in-process launch, which is what "non-TTY and
    /// `--no-session` behaviour remain supported" means as a decision rather
    /// than as a hope.
    #[test]
    fn chat_uses_the_runtime_only_with_the_gate_on_and_a_real_terminal() {
        assert_eq!(chat_route(true, false, true, true), ChatRoute::Runtime);
        assert_eq!(
            chat_route(false, false, true, true),
            ChatRoute::InProcess,
            "the gate is off by default"
        );
        assert_eq!(
            chat_route(true, true, true, true),
            ChatRoute::InProcess,
            "--no-session is the escape hatch"
        );
        assert_eq!(
            chat_route(true, false, false, true),
            ChatRoute::InProcess,
            "a piped stdin has nothing to attach"
        );
        assert_eq!(
            chat_route(true, false, true, false),
            ChatRoute::InProcess,
            "a redirected stdout has nowhere to paint"
        );
    }

    /// With the gate ON but no runtime listening, a client verb says how to
    /// start one instead of failing obscurely -- and still never exits 0,
    /// because nothing was done.
    #[test]
    fn a_client_verb_without_a_runtime_says_how_to_start_one() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let env = env_with(&[
            (
                "ZIRV_CTX_STATE_DIR",
                &tmp.path().to_string_lossy().into_owned(),
            ),
            ("ZIRV_CTX_SESSION_PERSISTENT", "true"),
        ]);
        let cli = SessionCli::try_parse_from(["zirv session", "list"]).expect("parse");
        let (code, text) = render(&cli, &env);
        assert_eq!(code, 1);
        assert!(text.contains("zirv session serve"), "{text}");
    }
}
