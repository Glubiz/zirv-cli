//! Re-expresses the `codex exec` launch exec built as the same turn over
//! `codex app-server`: the same program, prefix, env and cwd. `exec`'s config
//! flags move onto the server argv, its sandbox and model onto `thread/start`.
//! A flag with no verified app-server equivalent is a fall-back reason.

use std::process::Command;

use serde_json::{Map, Value};

use super::probe::FallBackReason;

/// One `codex exec` launch, re-expressed for the app-server.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Arguments before `exec`: the adapter's bin args and global overrides.
    pub prefix: Vec<String>,
    /// Config flags, appended after `app-server --listen stdio://`.
    pub server_flags: Vec<String>,
    /// `thread/start` params.
    pub thread: Value,
    pub prompt: String,
}

fn unsupported(what: impl Into<String>) -> FallBackReason {
    FallBackReason::Unsupported(what.into())
}

/// `--flag=value` or `--flag value`.
fn flag_value(
    inline: Option<&str>,
    rest: &mut std::slice::Iter<'_, String>,
    name: &str,
) -> Result<String, FallBackReason> {
    inline
        .map(str::to_string)
        .or_else(|| rest.next().cloned())
        .ok_or_else(|| unsupported(format!("{name} without a value")))
}

/// The prompt is `stdin_prompt` or the first argument after `exec`.
pub fn plan(command: &Command, stdin_prompt: Option<&str>) -> Result<Plan, FallBackReason> {
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let Some(exec) = args.iter().position(|arg| arg == "exec") else {
        return Err(unsupported("a launch without `exec`"));
    };
    let mut rest = args.get(exec + 1..).unwrap_or_default().iter();
    let prompt = match stdin_prompt {
        Some(prompt) => prompt.to_string(),
        None => rest
            .next()
            .cloned()
            .ok_or_else(|| unsupported("an `exec` launch without a prompt"))?,
    };
    let mut server_flags = Vec::new();
    let mut thread = Map::new();
    // `codex exec`'s own default; an explicit approval flag below replaces it.
    thread.insert("approvalPolicy".into(), "never".into());
    if let Some(cwd) = command.get_current_dir() {
        thread.insert("cwd".into(), cwd.display().to_string().into());
    }
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (arg.as_str(), None),
        };
        match name {
            "-c" | "--config" | "--enable" | "--disable" => {
                let value = flag_value(inline, &mut rest, name)?;
                server_flags.extend([name.to_string(), value]);
            }
            "--strict-config" => server_flags.push(name.to_string()),
            "-s" | "--sandbox" => {
                let value = flag_value(inline, &mut rest, name)?;
                thread.insert("sandbox".into(), value.into());
            }
            "-m" | "--model" => {
                let value = flag_value(inline, &mut rest, name)?;
                thread.insert("model".into(), value.into());
            }
            "-a" | "--ask-for-approval" => {
                let value = flag_value(inline, &mut rest, name)?;
                thread.insert("approvalPolicy".into(), value.into());
            }
            _ => return Err(unsupported(arg.clone())),
        }
    }
    Ok(Plan {
        prefix: args.get(..exec).unwrap_or_default().to_vec(),
        server_flags,
        thread: Value::Object(thread),
        prompt,
    })
}

/// `<program> <prefix> app-server --listen stdio:// <server flags>`, with the
/// exec launch's env and cwd.
pub fn server_command(exec_command: &Command, plan: &Plan) -> Command {
    let mut command = Command::new(exec_command.get_program());
    command
        .args(&plan.prefix)
        .args(["app-server", "--listen", "stdio://"])
        .args(&plan.server_flags);
    for (key, value) in exec_command.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    if let Some(dir) = exec_command.get_current_dir() {
        command.current_dir(dir);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsStr;

    fn exec(args: &[&str]) -> Command {
        let mut command = Command::new("codex");
        command.args(args).current_dir("/repo");
        command
    }

    fn argv(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn an_exec_launch_maps_onto_server_flags_and_thread_params() {
        let mut command = exec(&[
            "-c",
            "model_provider=\"x\"",
            "exec",
            "do the work",
            "--sandbox",
            "workspace-write",
            "-c",
            "approval_policy=never",
            "--model=gpt-x",
            "--enable",
            "feat",
            "--strict-config",
        ]);
        command.env("CODEX_HOME", "/h").env_remove("GONE");
        let plan = plan(&command, None).expect("plan");
        assert_eq!(plan.prefix, ["-c", "model_provider=\"x\""]);
        assert_eq!(
            plan.server_flags,
            [
                "-c",
                "approval_policy=never",
                "--enable",
                "feat",
                "--strict-config"
            ]
        );
        assert_eq!(
            plan.thread,
            json!({"approvalPolicy": "never", "cwd": "/repo", "sandbox": "workspace-write", "model": "gpt-x"})
        );
        assert_eq!(plan.prompt, "do the work");

        let server = server_command(&command, &plan);
        assert_eq!(server.get_program(), "codex");
        assert_eq!(
            argv(&server),
            [
                "-c",
                "model_provider=\"x\"",
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "approval_policy=never",
                "--enable",
                "feat",
                "--strict-config"
            ]
        );
        let envs: Vec<_> = server.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("CODEX_HOME"), Some(OsStr::new("/h")))));
        assert!(envs.contains(&(OsStr::new("GONE"), None)));
        assert_eq!(
            server.get_current_dir(),
            Some(std::path::Path::new("/repo"))
        );
    }

    #[test]
    fn a_stdin_prompt_leaves_every_argument_after_exec_a_flag() {
        let plan = plan(
            &exec(&[
                "exec",
                "-s",
                "read-only",
                "--ask-for-approval",
                "on-request",
            ]),
            Some("from stdin"),
        )
        .expect("plan");
        assert_eq!(plan.prompt, "from stdin");
        assert_eq!(plan.thread["sandbox"], "read-only");
        assert_eq!(plan.thread["approvalPolicy"], "on-request");
    }

    #[test]
    fn anything_without_a_verified_equivalent_falls_back() {
        let cases: [(&[&str], &str); 4] = [
            (&["exec", "p", "--ignore-rules"], "--ignore-rules"),
            (&["exec", "p", "resume", "id"], "resume"),
            (&["exec", "p", "--sandbox"], "--sandbox without a value"),
            (&["mcp", "list"], "a launch without `exec`"),
        ];
        for (args, why) in cases {
            assert_eq!(
                plan(&exec(args), None),
                Err(FallBackReason::Unsupported(why.to_string())),
                "{args:?}"
            );
        }
        assert_eq!(
            plan(&exec(&["exec"]), None),
            Err(FallBackReason::Unsupported(
                "an `exec` launch without a prompt".to_string()
            ))
        );
    }
}
