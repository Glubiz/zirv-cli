//! Exercise the real stdio transport, not just the server's in-process methods.

use super::*;
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Repository/worktree to check. Defaults to the launch directory.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Registered inbox session; defaults to inherited ZIRV_CTX_SESSION.
    #[arg(long)]
    session: Option<String>,
    /// Deadline for discovery and tool invocation, in seconds.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
    timeout_seconds: u64,
}

pub(super) fn run(args: &DoctorArgs) -> CtxResult<i32> {
    let repo = args
        .repo
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?
        .canonicalize()?;
    let session = args.session.clone();
    let timeout = Duration::from_secs(args.timeout_seconds);
    let executable = std::env::current_exe()?;
    let report = std::thread::spawn(move || -> Result<Value, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        runtime.block_on(check(&executable, &repo, session.as_deref(), timeout))
    })
    .join()
    .map_err(|_| "MCP diagnostic thread failed")??;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(0)
}

async fn check(
    executable: &Path,
    repo: &Path,
    session: Option<&str>,
    timeout: Duration,
) -> Result<Value, String> {
    let mut command = tokio::process::Command::new(executable);
    command.args(["ctx", "mcp", "serve", "--repo"]).arg(repo);
    if let Some(session) = session {
        command.arg("--session").arg(session);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let operation = async {
        let stdin = child.stdin.take().ok_or("diagnostic child has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("diagnostic child has no stdout")?;
        let service =
            ().serve((stdout, stdin))
                .await
                .map_err(|e| format!("MCP initialization failed: {e}"))?;
        let listed = service
            .list_tools(None)
            .await
            .map_err(|e| format!("MCP discovery failed: {e}"))?;
        let mut names: Vec<_> = listed
            .tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        let mut expected: Vec<_> = tools().iter().map(|tool| tool.name.to_string()).collect();
        expected.sort();
        if names != expected || listed.tools.iter().any(|tool| tool.output_schema.is_none()) {
            return Err("MCP discovery returned an unexpected tool contract".into());
        }
        let response = service
            .call_tool(CallToolRequestParams::new("session_snapshot"))
            .await
            .map_err(|e| format!("MCP tool call failed: {e}"))?;
        if response.is_error == Some(true) {
            return Err(format!(
                "MCP session_snapshot refused: {}",
                serde_json::to_string(&response.content).unwrap_or_default()
            ));
        }
        let snapshot = response
            .structured_content
            .ok_or("MCP snapshot omitted structured data")?;
        if snapshot["repository"].as_str() != Some(repo.to_string_lossy().as_ref())
            || !snapshot["data"]["sessions"].is_array()
        {
            return Err("MCP snapshot returned the wrong repository or an invalid shape".into());
        }
        service
            .cancel()
            .await
            .map_err(|e| format!("MCP shutdown failed: {e}"))?;
        Ok(serde_json::json!({
            "ok": true,
            "executable": executable,
            "repository": repo,
            "transport": "stdio",
            "tools": names,
            "inbox_session": snapshot["data"]["inbox_session"],
            "checks": ["initialize", "tools/list", "session_snapshot"],
            "host_registration": "not inspected; configure this executable and repository in your MCP host"
        }))
    };
    let result = match tokio::time::timeout(timeout, operation).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "MCP diagnostic timed out after {} seconds",
            timeout.as_secs()
        )),
    };
    // Reap on every path, including timeout and failed negotiation. This probe
    // owns its child; it never changes an existing host's MCP process.
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn diagnostic_deadline_terminates_and_reaps_a_silent_server() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("silent-server");
        // Exec preserves the child PID, so this fixture has no orphan grandchild.
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let result = runtime.block_on(check(&script, tmp.path(), None, Duration::from_millis(100)));
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
