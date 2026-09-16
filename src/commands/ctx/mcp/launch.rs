//! Per-launch host registration. No project or user host config is edited.

use super::*;
use sha2::{Digest, Sha256};

const OPT_OUT: &str = "ZIRV_CTX_MCP_AUTOREGISTER";

/// Best effort: an unavailable bridge must never prevent a harness launch.
/// Callers pass the stable registry address, which survives transcript restarts.
pub(crate) fn arguments(
    agent: &str,
    repo: &Path,
    state: &StateDir,
    session: &str,
    existing: &[String],
) -> Vec<String> {
    if !matches!(agent, "claude" | "codex")
        || std::env::var(OPT_OUT).is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off"))
        || (agent == "claude" && existing.iter().any(|a| a == "--strict-mcp-config"))
    {
        return Vec::new();
    }
    let result = (|| {
        let executable = std::env::current_exe()?;
        let repo = repo.canonicalize()?;
        let args = build(agent, &executable, &repo, state, session, cfg!(windows))?;
        // A generated path with shell metacharacters must not turn an optional
        // bridge into a failed host launch on Windows npm installations.
        #[cfg(windows)]
        {
            let mut shim_args = vec!["/c".into(), "host.cmd".into()];
            shim_args.extend(args.iter().cloned());
            super::super::adapters::guard_cmd_shim_reparse("cmd.exe", &shim_args)?;
        }
        Ok::<_, Box<dyn std::error::Error>>(args)
    })();
    match result {
        Ok(args) => args,
        Err(error) => {
            eprintln!("zirv: automatic MCP registration unavailable: {error}");
            Vec::new()
        }
    }
}

fn build(
    agent: &str,
    executable: &Path,
    repo: &Path,
    state: &StateDir,
    session: &str,
    config_file: bool,
) -> CtxResult<Vec<String>> {
    let executable = executable.to_str().ok_or("non-UTF-8 executable path")?;
    let repo = repo.to_str().ok_or("non-UTF-8 repository path")?;
    let state_path = std::path::absolute(state.root())?;
    let state_root = state_path.to_str().ok_or("non-UTF-8 state path")?;
    let args = ["ctx", "mcp", "serve", "--repo", repo, "--session", session];
    if agent == "claude" {
        let config = serde_json::json!({"mcpServers": {"zirv": {
            "type": "stdio", "command": executable, "args": args,
            "env": {"ZIRV_CTX_STATE_DIR": state_root}
        }}});
        let json = serde_json::to_string(&config)?;
        let value = if config_file {
            // JSON quotes cannot safely travel through an npm cmd.exe shim.
            // Content-addressing keeps concurrent seats and upgrades separate.
            let digest: String = Sha256::digest(json.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let dir = state_path.join("mcp-launch");
            super::super::state::create_private_dir_all(&dir)?;
            let path = dir.join(format!("{digest}.json"));
            super::super::state::write_private(&path, &json)?;
            path.to_str()
                .ok_or("non-UTF-8 MCP config path")?
                .to_string()
        } else {
            json
        };
        // Registration alone leaves MCP tools denied under Claude's headless
        // dontAsk mode. Approve only this binary's declared read-only tools;
        // native deny/ask rules and the server's per-call policy still apply.
        let allowed = super::tools()
            .into_iter()
            .filter(|tool| {
                tool.annotations
                    .as_ref()
                    .is_some_and(|a| a.read_only_hint == Some(true))
            })
            .map(|tool| format!("mcp__zirv__{}", tool.name))
            .collect::<Vec<_>>()
            .join(",");
        return Ok(vec![
            format!("--mcp-config={value}"),
            format!("--allowedTools={allowed}"),
        ]);
    }

    // Forward names, never credential values on argv. Codex filters its MCP
    // child's environment; retain operator policy overrides as well as its
    // home/config locations. The supervisor provides the state root explicitly.
    let mut forwarded = vec![
        "HOME".to_string(),
        "USERPROFILE".into(),
        "APPDATA".into(),
        "LOCALAPPDATA".into(),
        "XDG_CONFIG_HOME".into(),
        "XDG_STATE_HOME".into(),
    ];
    forwarded.extend(
        std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter(|k| k.starts_with("ZIRV_CTX_") && k != super::super::adapters::SESSION_ENV),
    );
    forwarded.sort();
    forwarded.dedup();
    let array = |values: &[&str]| {
        values
            .iter()
            .map(|v| quoted(v))
            .collect::<Vec<_>>()
            .join(",")
    };
    let forwarded: Vec<_> = forwarded.iter().map(String::as_str).collect();
    // Replace only our named server, never the mcp_servers table. A complete
    // entry also avoids mixing a stale HTTP registration with a stdio one.
    Ok(vec![
        "-c".into(),
        format!(
            "mcp_servers.zirv={{command={},args=[{}],env={{ZIRV_CTX_STATE_DIR={}}},env_vars=[{}]}}",
            quoted(executable),
            array(&args),
            quoted(state_root),
            array(&forwarded)
        ),
    ])
}

fn quoted(value: &str) -> String {
    if !value.contains('\'') && !value.chars().any(char::is_control) {
        format!("'{value}'")
    } else {
        // Rust strings serialize infallibly; serde_json's escapes are valid
        // TOML basic-string escapes for paths and environment variable names.
        serde_json::Value::String(value.to_string()).to_string()
    }
}

/// Append before an explicit end-of-options delimiter. In particular, a
/// Claude variadic MCP option must not consume a following positional prompt.
pub(crate) fn append(argv: &mut Vec<String>, mut args: Vec<String>) {
    let at = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    // Merge into the existing permission argument rather than depending on
    // a host version's treatment of repeated --allowedTools options.
    if let Some(index) = args
        .iter()
        .position(|arg| arg.starts_with("--allowedTools="))
    {
        let allowed = &args[index]["--allowedTools=".len()..];
        let existing = argv[..at].iter().rposition(|arg| {
            arg.starts_with("--allowedTools=")
                || arg.starts_with("--allowed-tools=")
                || arg == "--allowedTools"
                || arg == "--allowed-tools"
        });
        if let Some(existing) = existing {
            let value = if argv[existing].contains('=') {
                existing
            } else {
                existing + 1
            };
            if value < at && (value == existing || !argv[value].starts_with('-')) {
                argv[value].push(',');
                argv[value].push_str(allowed);
                args.remove(index);
            }
        }
    }
    argv.splice(at..at, args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_registration_is_one_server_and_round_trips_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        let executable = Path::new("C:\\Program Files\\zirv's\\zirv.exe");
        let repo = Path::new("C:\\work\\project with spaces");
        let args = build("codex", executable, repo, &state, "seat1234", false).unwrap();
        assert_eq!(args[0], "-c");
        let value: toml::Value = toml::from_str(&args[1]).unwrap();
        let servers = value["mcp_servers"].as_table().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["zirv"]["command"].as_str(), executable.to_str());
        let server_args = servers["zirv"]["args"].as_array().unwrap();
        assert_eq!(server_args[4].as_str(), repo.to_str());
        assert_eq!(server_args[6].as_str(), Some("seat1234"));
        assert_eq!(
            servers["zirv"]["env"]["ZIRV_CTX_STATE_DIR"].as_str(),
            state.root().to_str()
        );
    }

    #[test]
    fn claude_inline_registration_needs_no_file_and_preserves_other_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        let args = build(
            "claude",
            Path::new("/opt/zirv"),
            tmp.path(),
            &state,
            "seat1234",
            false,
        )
        .unwrap();
        let config: Value =
            serde_json::from_str(args[0].strip_prefix("--mcp-config=").unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["zirv"]["command"], "/opt/zirv");
        assert_eq!(config["mcpServers"]["zirv"]["args"][6], "seat1234");
        let mut argv = vec![
            "claude".into(),
            "--mcp-config=other.json".into(),
            "--".into(),
            "prompt".into(),
        ];
        append(&mut argv, args);
        assert_eq!(argv[1], "--mcp-config=other.json");
        let allowed = argv[3].strip_prefix("--allowedTools=").unwrap();
        let names: Vec<_> = allowed.split(',').collect();
        assert_eq!(names.len(), 7);
        assert!(names.contains(&"mcp__zirv__inbox_read"));
        assert!(
            names
                .iter()
                .all(|name| name.starts_with("mcp__zirv__") && !name.contains('*'))
        );
        assert_eq!(&argv[4..], &["--", "prompt"]);
        assert!(!tmp.path().join(".mcp.json").exists());
        assert!(!state.root().exists());
    }

    #[test]
    fn registering_claude_keeps_existing_tool_permissions_in_one_argument() {
        for mut argv in [
            vec!["--allowedTools=Read,Bash(git status)".into()],
            vec!["--allowed-tools".into(), "Read,Bash(git status)".into()],
        ] {
            append(
                &mut argv,
                vec!["--allowedTools=mcp__zirv__inbox_read".into()],
            );
            assert_eq!(
                argv.iter().filter(|a| a.starts_with("--allowed")).count(),
                1
            );
            assert!(
                argv.last()
                    .unwrap()
                    .ends_with("Read,Bash(git status),mcp__zirv__inbox_read")
            );
        }
    }

    #[test]
    fn claude_shim_config_is_private_and_separate_for_each_seat() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        let a = build(
            "claude",
            Path::new("/opt/zirv"),
            tmp.path(),
            &state,
            "seat1234",
            true,
        )
        .unwrap();
        let b = build(
            "claude",
            Path::new("/opt/zirv"),
            tmp.path(),
            &state,
            "seat5678",
            true,
        )
        .unwrap();
        assert_ne!(a, b);
        let path = Path::new(a[0].strip_prefix("--mcp-config=").unwrap());
        let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["zirv"]["args"][6], "seat1234");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn unsupported_hosts_and_explicit_exclusive_claude_config_are_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        assert!(arguments("native", tmp.path(), &state, "seat1234", &[]).is_empty());
        assert!(
            arguments(
                "claude",
                tmp.path(),
                &state,
                "seat1234",
                &["--strict-mcp-config".into()]
            )
            .is_empty()
        );
    }

    #[test]
    fn generated_registration_connects_to_the_real_server_and_its_own_inbox() {
        use std::process::Stdio;
        use std::time::Duration;
        use tokio::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _home = super::super::super::testenv::HomeGuard::set(&home);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        let session = "12345678-1111-4111-8111-111111111111";
        let _guard = sessions::SessionGuard::register(
            &state,
            sessions::Record::new(session, "claude", &repo, sessions::Verb::Exec),
        );
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(if cfg!(windows) { "zirv.exe" } else { "zirv" });
        let args = build("claude", &binary, &repo, &state, "12345678", false).unwrap();
        let config: Value =
            serde_json::from_str(args[0].strip_prefix("--mcp-config=").unwrap()).unwrap();
        let server = &config["mcpServers"]["zirv"];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut command = Command::new(server["command"].as_str().unwrap());
            // Simulate a host with no inherited supervisor identity or state.
            command
                .env_clear()
                .env("HOME", &home)
                .env("USERPROFILE", &home);
            command.args(
                server["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap()),
            );
            for (key, value) in server["env"].as_object().unwrap() {
                command.env(key, value.as_str().unwrap());
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                let service =
                    ().serve((child.stdout.take().unwrap(), child.stdin.take().unwrap()))
                        .await
                        .unwrap();
                let response = service
                    .call_tool(CallToolRequestParams::new("session_snapshot"))
                    .await
                    .unwrap();
                assert_ne!(response.is_error, Some(true));
                let value = response.structured_content.unwrap();
                assert_eq!(value["repository"], repo.to_string_lossy().as_ref());
                assert_eq!(value["data"]["inbox_session"], session);
                service.cancel().await.unwrap();
            })
            .await;
            let _ = child.start_kill();
            let _ = child.wait().await;
            result.unwrap();
        });
        assert!(!repo.join(".mcp.json").exists());
        assert!(!home.join(".codex/config.toml").exists());
    }
}
