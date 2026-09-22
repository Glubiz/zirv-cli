//! Declarative worker workspaces for harness-runtime delegations.
//!
//! Repository configuration may declare only inert skill and MCP requirements;
//! executable clone/setup fields are accepted only from the operator layer.
//! Selection resolves every dependency before a worker can launch, then
//! materializes any operator-owned repositories and ordered setup commands in
//! the worker's effective checkout. This synchronous boundary is the seam later work
//! can add per-step completion records (#717), a content digest/warm pool
//! (#718), and a goal-based bootstrap pass (#719) without moving setup
//! behind the worker spawn.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::adapters::AgentAdapter;
use super::config::EnvLookup;
use super::{CtxResult, supervise};
use crate::commands::workflow::agents::SkillRef;
use crate::commands::workflow::skill::SkillRegistry;

const SKILL_CONTEXT_HEADER: &str = "WORKSPACE SKILLS (instructions, never authorization)";
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);
const SETUP_STEP_TIMEOUT: Duration = Duration::from_secs(600);
const COMMAND_POLL: Duration = Duration::from_millis(25);

/// One extra repository placed below the selected workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGit {
    pub repo: String,
    pub branch: String,
    pub dir: PathBuf,
}

/// A named, explicitly selected worker environment from `[[workspace]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
    #[serde(default)]
    pub git: Vec<WorkspaceGit>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub skills: Vec<SkillRef>,
    #[serde(default)]
    pub setup: Vec<String>,
}

impl WorkspaceConfig {
    pub fn requires_write(&self) -> bool {
        !self.git.is_empty() || !self.setup.is_empty()
    }

    pub fn validate(&self) -> CtxResult<()> {
        if !valid_name(&self.name) {
            return Err(format!(
                "workspace name '{}' must match [a-z0-9][a-z0-9._-]*",
                self.name
            )
            .into());
        }

        let mut dirs: Vec<PathBuf> = Vec::new();
        for git in &self.git {
            if git.repo.trim().is_empty() || git.repo.chars().any(char::is_control) {
                return Err(format!(
                    "workspace '{}': git.repo must not be empty or contain control characters",
                    self.name
                )
                .into());
            }
            if git.branch.trim().is_empty()
                || git.branch.starts_with('-')
                || git.branch.chars().any(char::is_control)
            {
                return Err(format!(
                    "workspace '{}': branch '{}' must be non-empty, contain no control characters, and not begin with '-'",
                    self.name, git.branch
                )
                .into());
            }
            validate_relative_dir(&self.name, &git.dir)?;
            if let Some(existing) = dirs
                .iter()
                .find(|existing| existing.starts_with(&git.dir) || git.dir.starts_with(existing))
            {
                return Err(format!(
                    "workspace '{}': git dirs '{}' and '{}' overlap",
                    self.name,
                    existing.display(),
                    git.dir.display()
                )
                .into());
            }
            dirs.push(git.dir.clone());
        }

        let mut servers = HashSet::new();
        for server in &self.mcp_servers {
            if server.trim().is_empty() || server.chars().any(char::is_control) {
                return Err(format!(
                    "workspace '{}': MCP server names must not be empty or contain control characters",
                    self.name
                )
                .into());
            }
            if !servers.insert(server) {
                return Err(format!(
                    "workspace '{}': MCP server '{}' is declared more than once",
                    self.name, server
                )
                .into());
            }
        }

        let mut skills = HashSet::new();
        for skill in &self.skills {
            if !valid_name(&skill.id) {
                return Err(format!(
                    "workspace '{}': skill id '{}' must match [a-z0-9][a-z0-9._-]*",
                    self.name, skill.id
                )
                .into());
            }
            if !skills.insert(&skill.id) {
                return Err(format!(
                    "workspace '{}': skill '{}' is referenced more than once",
                    self.name, skill.id
                )
                .into());
            }
        }

        for (index, step) in self.setup.iter().enumerate() {
            if step.trim().is_empty() || step.contains('\0') {
                return Err(format!(
                    "workspace '{}': setup step {} must not be empty or contain NUL",
                    self.name,
                    index + 1
                )
                .into());
            }
        }
        Ok(())
    }
}

/// Validate the full layered catalogue once after config deserialization.
pub fn validate_catalogue(workspaces: &[WorkspaceConfig]) -> CtxResult<()> {
    let mut names = HashSet::new();
    for workspace in workspaces {
        workspace.validate()?;
        if !names.insert(&workspace.name) {
            return Err(format!("duplicate workspace name '{}'", workspace.name).into());
        }
    }
    Ok(())
}

pub fn resolve<'a>(
    workspaces: &'a [WorkspaceConfig],
    name: &str,
) -> CtxResult<&'a WorkspaceConfig> {
    workspaces
        .iter()
        .find(|workspace| workspace.name == name)
        .ok_or_else(|| {
            let known = workspaces
                .iter()
                .map(|workspace| workspace.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "unknown workspace '{name}'{}",
                if known.is_empty() {
                    "; no [[workspace]] entries are configured".to_string()
                } else {
                    format!("; configured workspaces: {known}")
                }
            )
            .into()
        })
}

/// Workspace skills override manifest defaults only when the selected
/// workspace declares at least one. This is the stable resolution seam for
/// workflow manifests today and the bootstrap worker added by #719 later.
pub fn effective_skill_refs<'a>(
    workspace: Option<&'a WorkspaceConfig>,
    manifest_defaults: &'a [SkillRef],
) -> &'a [SkillRef] {
    workspace
        .filter(|workspace| !workspace.skills.is_empty())
        .map_or(manifest_defaults, |workspace| workspace.skills.as_slice())
}

/// Resolve every selected skill before a worktree is allocated. The same
/// registry is used later to render the instruction bodies, so no parallel
/// skill-loading path is introduced.
pub fn validate_skills(
    workspace: &WorkspaceConfig,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<()> {
    let home = home_dir(env);
    let registry = SkillRegistry::load_for_repo(repo, home.as_deref(), true)?;
    for requested in effective_skill_refs(Some(workspace), &[]) {
        let requested = skill_request(requested);
        let skill = registry
            .get(&requested)
            .map_err(|error| format!("workspace '{}': {error}", workspace.name))?;
        // Resolve the full dependency stack here, not only the root. A
        // missing/cyclic/oversized dependency is just as much a pre-launch
        // workspace failure as a missing root skill.
        registry
            .resolve_stack(&skill.manifest.id)
            .map_err(|error| format!("workspace '{}': {error}", workspace.name))?;
    }
    Ok(())
}

/// Render only explicitly selected skills (plus their dependencies) into a
/// labelled prompt block. Repository skills stay explicitly untrusted and
/// remain instructions rather than capability grants.
pub fn attach_skills(
    workspace: &WorkspaceConfig,
    repo: &Path,
    prompt: String,
    env: EnvLookup<'_>,
) -> CtxResult<String> {
    let requested_skills = effective_skill_refs(Some(workspace), &[]);
    if requested_skills.is_empty() {
        return Ok(prompt);
    }
    let home = home_dir(env);
    let registry = SkillRegistry::load_for_repo(repo, home.as_deref(), true)?;
    let mut seen = BTreeSet::new();
    let mut rendered = String::new();
    for requested in requested_skills {
        let requested = skill_request(requested);
        let root = registry
            .get(&requested)
            .map_err(|error| format!("workspace '{}': {error}", workspace.name))?;
        for skill in registry.resolve_stack(&root.manifest.id)? {
            if !seen.insert(skill.manifest.id.clone()) {
                continue;
            }
            rendered.push_str(&format!(
                "\n[skill {}@{}; source={}]\n{}\n",
                skill.manifest.id,
                skill.manifest.version,
                skill.source,
                skill.manifest.instructions.trim()
            ));
        }
    }
    Ok(format!(
        "{prompt}\n\n---\n{SKILL_CONTEXT_HEADER}\n{rendered}---\nEND WORKSPACE SKILLS"
    ))
}

/// The successful result of the synchronous pre-launch materialization gate.
/// Construction is private: a caller only receives this after MCP validation,
/// every clone, and every setup command succeeded.
#[derive(Debug)]
#[must_use]
pub struct WorkspaceReady {
    _private: (),
}

pub fn materialize(
    workspace: &WorkspaceConfig,
    root: &Path,
    adapter: &dyn AgentAdapter,
    adapter_flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<WorkspaceReady> {
    validate_mcp_servers(workspace, root, adapter, adapter_flags, env)?;
    clone_repositories(workspace, root)?;
    run_setup(workspace, root)?;
    Ok(WorkspaceReady { _private: () })
}

fn validate_mcp_servers(
    workspace: &WorkspaceConfig,
    root: &Path,
    adapter: &dyn AgentAdapter,
    adapter_flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<()> {
    if workspace.mcp_servers.is_empty() {
        return Ok(());
    }
    let configured = adapter.configured_mcp_servers(root, adapter_flags, env)?;
    let missing: Vec<_> = workspace
        .mcp_servers
        .iter()
        .filter(|name| !configured.contains(name.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "workspace '{}': adapter '{}' has no configured MCP server(s): {}; refusing delegation before worker launch",
        workspace.name,
        adapter.name(),
        missing.join(", ")
    )
    .into())
}

fn clone_repositories(workspace: &WorkspaceConfig, root: &Path) -> CtxResult<()> {
    for git in &workspace.git {
        let destination = secure_destination(workspace, root, &git.dir)?;
        if std::fs::symlink_metadata(&destination).is_ok() {
            validate_existing_clone(workspace, git, &destination)?;
            continue;
        }
        let mut command = Command::new("git");
        command
            .arg("clone")
            .arg("--single-branch")
            .arg("--branch")
            .arg(&git.branch)
            .arg("--")
            .arg(&git.repo)
            .arg(&destination)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let outcome = run_bounded(
            &mut command,
            CLONE_TIMEOUT,
            &[("GIT_TERMINAL_PROMPT", "0")],
        )
        .map_err(|error| {
                format!(
                    "workspace '{}': could not run git clone for '{}': {error}",
                    workspace.name,
                    git.dir.display()
                )
            })?;
        match outcome {
            supervise::Outcome::Exited(0) => {}
            supervise::Outcome::Exited(code) => {
                return Err(format!(
                    "workspace '{}': git clone into '{}' failed with exit code {code}; repository URL and subprocess output withheld because they may contain credentials",
                    workspace.name,
                    git.dir.display(),
                )
                .into());
            }
            supervise::Outcome::TimedOut => {
                return Err(format!(
                    "workspace '{}': git clone into '{}' timed out after {} seconds; refusing delegation before worker launch",
                    workspace.name,
                    git.dir.display(),
                    CLONE_TIMEOUT.as_secs(),
                )
                .into());
            }
            supervise::Outcome::StoppedByTick(_) => unreachable!("workspace commands never stop by tick"),
        }
    }
    Ok(())
}

/// Resolve a clone destination while rejecting symlinks in every existing
/// component. Parents are created one at a time only after their predecessor
/// was verified as a real directory below the canonical workspace root.
fn secure_destination(
    workspace: &WorkspaceConfig,
    root: &Path,
    relative: &Path,
) -> CtxResult<PathBuf> {
    let canonical_root = std::fs::canonicalize(root).map_err(|error| {
        format!(
            "workspace '{}': could not resolve workspace root '{}': {error}",
            workspace.name,
            root.display()
        )
    })?;
    if !canonical_root.is_dir() {
        return Err(format!(
            "workspace '{}': workspace root '{}' is not a directory",
            workspace.name,
            root.display()
        )
        .into());
    }
    let components: Vec<OsString> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect();
    let mut current = canonical_root.clone();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let is_destination = index + 1 == components.len();
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "workspace '{}': refusing git dir '{}' because '{}' is a symlink",
                        workspace.name,
                        relative.display(),
                        current.display()
                    )
                    .into());
                }
                if !is_destination && !metadata.is_dir() {
                    return Err(format!(
                        "workspace '{}': git dir '{}' has non-directory ancestor '{}'",
                        workspace.name,
                        relative.display(),
                        current.display()
                    )
                    .into());
                }
                let resolved = std::fs::canonicalize(&current)?;
                if !resolved.starts_with(&canonical_root) {
                    return Err(format!(
                        "workspace '{}': git dir '{}' escapes workspace root through '{}'",
                        workspace.name,
                        relative.display(),
                        current.display()
                    )
                    .into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_destination => {
                std::fs::create_dir(&current).map_err(|error| {
                    format!(
                        "workspace '{}': could not create parent '{}' for git dir '{}': {error}",
                        workspace.name,
                        current.display(),
                        relative.display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(canonical_root.join(relative))
}

fn validate_existing_clone(
    workspace: &WorkspaceConfig,
    git: &WorkspaceGit,
    destination: &Path,
) -> CtxResult<()> {
    if std::fs::symlink_metadata(destination)?
        .file_type()
        .is_symlink()
    {
        return Err(format!(
            "workspace '{}': refusing symlinked git dir '{}'",
            workspace.name,
            git.dir.display()
        )
        .into());
    }
    let origin = git_output(destination, &["remote", "get-url", "origin"])?;
    if normalize_repo(&origin) != normalize_repo(&git.repo) {
        return Err(format!(
            "workspace '{}': existing git dir '{}' has a different origin",
            workspace.name,
            git.dir.display()
        )
        .into());
    }
    let branch = git_output(destination, &["branch", "--show-current"])?;
    if branch != git.branch {
        return Err(format!(
            "workspace '{}': existing git dir '{}' is on branch '{}', expected '{}'",
            workspace.name,
            git.dir.display(),
            branch,
            git.branch
        )
        .into());
    }
    Ok(())
}

fn git_output(cwd: &Path, args: &[&str]) -> CtxResult<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    apply_workspace_environment(&mut command, std::env::vars_os());
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "workspace git validation failed in '{}' with status {}",
            cwd.display(),
            output.status
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_setup(workspace: &WorkspaceConfig, root: &Path) -> CtxResult<()> {
    run_setup_with_timeout(workspace, root, SETUP_STEP_TIMEOUT)
}

fn run_setup_with_timeout(
    workspace: &WorkspaceConfig,
    root: &Path,
    timeout: Duration,
) -> CtxResult<()> {
    for (index, command) in workspace.setup.iter().enumerate() {
        let mut shell = if cfg!(windows) {
            let mut shell = Command::new("powershell");
            shell.arg("-Command").arg(command);
            shell
        } else {
            let mut shell = Command::new("sh");
            shell.arg("-c").arg(command);
            shell
        };
        shell.current_dir(root);
        let outcome = run_bounded(&mut shell, timeout, &[]).map_err(|error| {
            format!(
                "workspace '{}': setup step {} could not start: {error}",
                workspace.name,
                index + 1
            )
        })?;
        match outcome {
            supervise::Outcome::Exited(0) => {}
            supervise::Outcome::Exited(code) => {
                return Err(format!(
                    "workspace '{}': setup step {} failed with exit code {code}; refusing delegation before worker launch",
                    workspace.name,
                    index + 1
                )
                .into());
            }
            supervise::Outcome::TimedOut => {
                return Err(format!(
                    "workspace '{}': setup step {} timed out after {} seconds; refusing delegation before worker launch",
                    workspace.name,
                    index + 1,
                    timeout.as_secs(),
                )
                .into());
            }
            supervise::Outcome::StoppedByTick(_) => unreachable!("workspace commands never stop by tick"),
        }
    }
    Ok(())
}

fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    environment_overrides: &[(&str, &str)],
) -> CtxResult<supervise::Outcome> {
    apply_workspace_environment(command, std::env::vars_os());
    command.envs(environment_overrides.iter().copied());
    command.stdin(Stdio::null());
    supervise::isolate_process_tree(command);
    let mut child = command.spawn()?;
    let mut keep_running = || supervise::Tick::Continue;
    supervise::supervise_child(
        &mut child,
        Instant::now() + timeout,
        COMMAND_POLL,
        &mut keep_running,
    )
}

fn apply_workspace_environment<I>(command: &mut Command, environment: I)
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    command.env_clear();
    command.envs(workspace_environment(environment));
}

/// Keep ordinary process context while removing credentials, Zirv authority
/// envelopes/session metadata, and inherited git control variables. This is
/// defense in depth even for operator-authored commands.
fn workspace_environment<I>(environment: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let value = value.into_string().ok()?;
            let upper = key.to_ascii_uppercase();
            let allowed = matches!(
                upper.as_str(),
                "PATH"
                    | "PATHEXT"
                    | "SYSTEMROOT"
                    | "WINDIR"
                    | "COMSPEC"
                    | "HOME"
                    | "USERPROFILE"
                    | "HOMEDRIVE"
                    | "HOMEPATH"
                    | "TEMP"
                    | "TMP"
                    | "TMPDIR"
                    | "LANG"
                    | "LANGUAGE"
                    | "TZ"
                    | "TERM"
                    | "COLORTERM"
                    | "NO_COLOR"
                    | "SHELL"
                    | "USER"
                    | "LOGNAME"
            ) || upper.starts_with("LC_");
            allowed.then_some((key, value))
        })
        .collect()
}

/// Default adapter-side discovery used by the trait method. Claude and Codex
/// are deliberately explicit: guessing another harness's config format would
/// turn an unverified server into permission to launch.
pub(crate) fn configured_mcp_servers(
    adapter: &str,
    repo: &Path,
    flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<BTreeSet<String>> {
    match adapter {
        "claude" => claude_mcp_servers(repo, flags, env),
        "codex" => codex_mcp_servers(repo, flags, env),
        other => Err(format!(
            "workspace MCP validation is not implemented for adapter '{other}'; refusing delegation rather than assuming the server exists"
        )
        .into()),
    }
}

/// Whether MCP discovery for this invocation depends on launch-only adapter
/// flags. Dashboard request files deliberately discard arbitrary trailing
/// flags at their untrusted boundary, so a delegation using one of these
/// overrides must stay inline; otherwise the pre-launch check could approve
/// a server configuration the eventual pane child never receives.
pub(crate) fn uses_launch_scoped_mcp_config(adapter: &str, flags: &[String]) -> bool {
    match adapter {
        "claude" => flags.iter().any(|flag| {
            flag == "--mcp-config"
                || flag.starts_with("--mcp-config=")
                || flag == "--strict-mcp-config"
        }),
        "codex" => {
            let mut index = 0;
            while index < flags.len() {
                let value = if flags[index] == "-c" || flags[index] == "--config" {
                    index += 1;
                    flags.get(index).map(String::as_str)
                } else {
                    flags[index]
                        .strip_prefix("--config=")
                        .or_else(|| flags[index].strip_prefix("-c="))
                };
                if value.is_some_and(|value| value.trim_start().starts_with("mcp_servers.")) {
                    return true;
                }
                index += 1;
            }
            false
        }
        _ => false,
    }
}

fn claude_mcp_servers(
    repo: &Path,
    flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let strict = flags.iter().any(|flag| flag == "--strict-mcp-config");
    if !strict {
        read_json_mcp_file(&repo.join(".mcp.json"), Some(repo), &mut names)?;
        if let Some(home) = home_dir(env) {
            read_json_mcp_file(&home.join(".claude.json"), Some(repo), &mut names)?;
        }
    }
    for value in flag_values(flags, "--mcp-config") {
        if value.trim_start().starts_with('{') {
            collect_json_mcp_names(
                &serde_json::from_str(value).map_err(|error| {
                    format!("adapter 'claude': invalid inline --mcp-config JSON: {error}")
                })?,
                Some(repo),
                &mut names,
            );
        } else {
            let path = resolve_config_path(repo, value);
            read_json_mcp_file(&path, Some(repo), &mut names)?;
        }
    }
    Ok(names)
}

fn codex_mcp_servers(
    repo: &Path,
    flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    if let Some(home) = env("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir(env).map(|h| h.join(".codex")))
    {
        read_toml_mcp_file(&home.join("config.toml"), &mut names)?;
    }
    read_toml_mcp_file(&repo.join(".codex/config.toml"), &mut names)?;

    let mut index = 0;
    while index < flags.len() {
        let value = if flags[index] == "-c" || flags[index] == "--config" {
            index += 1;
            flags.get(index).map(String::as_str)
        } else {
            flags[index]
                .strip_prefix("--config=")
                .or_else(|| flags[index].strip_prefix("-c="))
        };
        if let Some(value) = value
            && value.trim_start().starts_with("mcp_servers.")
        {
            let parsed: toml::Table = value.parse().map_err(|error| {
                format!("adapter 'codex': invalid MCP config override: {error}")
            })?;
            collect_toml_mcp_names(&parsed, &mut names);
        }
        index += 1;
    }
    Ok(names)
}

fn read_json_mcp_file(
    path: &Path,
    repo: Option<&Path>,
    names: &mut BTreeSet<String>,
) -> CtxResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read MCP config '{}': {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid MCP config '{}': {error}", path.display()))?;
    collect_json_mcp_names(&value, repo, names);
    Ok(())
}

fn collect_json_mcp_names(
    value: &serde_json::Value,
    repo: Option<&Path>,
    names: &mut BTreeSet<String>,
) {
    if let Some(servers) = value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    {
        names.extend(servers.keys().cloned());
    }
    let Some(repo) = repo else { return };
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    if let Some(projects) = value.get("projects").and_then(serde_json::Value::as_object) {
        for (path, project) in projects {
            let candidate = PathBuf::from(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path));
            if candidate == canonical
                && let Some(servers) = project
                    .get("mcpServers")
                    .and_then(serde_json::Value::as_object)
            {
                names.extend(servers.keys().cloned());
            }
        }
    }
}

fn read_toml_mcp_file(path: &Path, names: &mut BTreeSet<String>) -> CtxResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read MCP config '{}': {error}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|error| format!("invalid MCP config '{}': {error}", path.display()))?;
    collect_toml_mcp_names(&table, names);
    Ok(())
}

fn collect_toml_mcp_names(table: &toml::Table, names: &mut BTreeSet<String>) {
    if let Some(toml::Value::Table(servers)) = table.get("mcp_servers") {
        names.extend(servers.keys().cloned());
    }
}

fn flag_values<'a>(flags: &'a [String], name: &str) -> Vec<&'a str> {
    let mut values = Vec::new();
    let prefix = format!("{name}=");
    let mut index = 0;
    while index < flags.len() {
        if flags[index] == name {
            index += 1;
            if let Some(value) = flags.get(index) {
                values.push(value.as_str());
            }
        } else if let Some(value) = flags[index].strip_prefix(&prefix) {
            values.push(value);
        }
        index += 1;
    }
    values
}

fn resolve_config_path(repo: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

fn home_dir(env: EnvLookup<'_>) -> Option<PathBuf> {
    env("HOME")
        .or_else(|| env("USERPROFILE"))
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

fn validate_relative_dir(workspace: &str, dir: &Path) -> CtxResult<()> {
    let mut components = dir.components();
    let Some(first) = components.next() else {
        return Err(format!("workspace '{workspace}': git.dir must not be empty").into());
    };
    let targets_zirv = first
        .as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(crate::utils::SCRIPT_DIR_NAME);
    if !matches!(first, Component::Normal(_))
        || components.any(|component| !matches!(component, Component::Normal(_)))
        || targets_zirv
    {
        return Err(format!(
            "workspace '{workspace}': git.dir '{}' must be a relative child path outside .zirv",
            dir.display()
        )
        .into());
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn skill_request(reference: &SkillRef) -> String {
    reference.version.map_or_else(
        || reference.id.clone(),
        |version| format!("{}@{version}", reference.id),
    )
}

fn normalize_repo(value: &str) -> &str {
    value.trim().trim_end_matches('/').trim_end_matches(".git")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> WorkspaceConfig {
        WorkspaceConfig {
            name: "dev".into(),
            git: Vec::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            setup: Vec::new(),
        }
    }

    #[test]
    fn catalogue_rejects_duplicate_names() {
        let error = validate_catalogue(&[workspace(), workspace()]).expect_err("duplicate");
        assert!(error.to_string().contains("duplicate workspace name 'dev'"));
    }

    #[test]
    fn workspace_and_nested_git_reject_unknown_fields() {
        let top = r#"name = "dev"
unknown = true
"#;
        assert!(toml::from_str::<WorkspaceConfig>(top).is_err());

        let nested = r#"name = "dev"
git = [{ repo = "https://example.test/repo", branch = "main", dir = "dep", extra = true }]
"#;
        assert!(toml::from_str::<WorkspaceConfig>(nested).is_err());
    }

    #[test]
    fn overlapping_git_destinations_are_rejected() {
        let mut config = workspace();
        config.git = vec![
            WorkspaceGit {
                repo: "https://example.test/one".into(),
                branch: "main".into(),
                dir: "deps".into(),
            },
            WorkspaceGit {
                repo: "https://example.test/two".into(),
                branch: "main".into(),
                dir: "deps/two".into(),
            },
        ];
        let error = config.validate().expect_err("overlapping destinations");
        assert!(
            error
                .to_string()
                .contains("git dirs 'deps' and 'deps/two' overlap")
        );
    }

    #[test]
    fn git_destinations_cannot_target_zirv_metadata_with_different_case() {
        let mut config = workspace();
        config.git.push(WorkspaceGit {
            repo: "https://example.test/repo".into(),
            branch: "main".into(),
            dir: ".ZIRV/vendor".into(),
        });
        let error = config.validate().expect_err("reserved metadata directory");
        assert!(error.to_string().contains("outside .zirv"));
    }

    #[test]
    fn launch_scoped_mcp_flags_are_identified_for_dashboard_safety() {
        assert!(uses_launch_scoped_mcp_config(
            "claude",
            &["--mcp-config=config.json".into()]
        ));
        assert!(uses_launch_scoped_mcp_config(
            "codex",
            &["-c".into(), "mcp_servers.docs.command='docs'".into()]
        ));
        assert!(!uses_launch_scoped_mcp_config(
            "codex",
            &["--model=gpt-5".into()]
        ));
    }

    #[test]
    fn manifest_skills_are_the_default_and_workspace_skills_override_them() {
        let defaults = vec![SkillRef {
            id: "default".into(),
            version: None,
        }];
        let empty = workspace();
        assert_eq!(effective_skill_refs(Some(&empty), &defaults), defaults);

        let mut selected = workspace();
        selected.skills.push(SkillRef {
            id: "workspace".into(),
            version: Some(2),
        });
        assert_eq!(
            effective_skill_refs(Some(&selected), &defaults),
            selected.skills
        );
    }

    #[test]
    fn codex_mcp_discovery_reads_files_and_cli_overrides() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).expect("mkdir");
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = \"docs\"\n",
        )
        .expect("config");
        let flags = vec![
            "-c".to_string(),
            "mcp_servers.linear={command='linear'}".to_string(),
        ];
        let names = codex_mcp_servers(temp.path(), &flags, &|key| {
            (key == "HOME").then(|| home.display().to_string())
        })
        .expect("discover");
        assert_eq!(names, BTreeSet::from(["docs".into(), "linear".into()]));
    }

    #[test]
    fn failed_setup_stops_before_later_steps() {
        if cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = workspace();
        config.setup = vec!["false".into(), "touch should-not-exist".into()];
        let error = run_setup(&config, temp.path()).expect_err("first step fails");
        assert!(error.to_string().contains("setup step 1 failed"));
        assert!(!temp.path().join("should-not-exist").exists());
    }

    #[test]
    fn workspace_subprocess_environment_excludes_credentials_and_zirv_authority() {
        let environment = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
            (OsString::from("CUSTOM_CREDENTIAL"), OsString::from("secret")),
            (OsString::from("ZIRV_ENVELOPE"), OsString::from("authority")),
            (OsString::from("ZIRV_CTX_SESSION"), OsString::from("session")),
            (OsString::from("GIT_DIR"), OsString::from("elsewhere")),
        ];
        let scrubbed = workspace_environment(environment);
        assert_eq!(scrubbed.get("PATH").map(String::as_str), Some("/bin"));
        for forbidden in [
            "OPENAI_API_KEY",
            "GH_TOKEN",
            "CUSTOM_CREDENTIAL",
            "ZIRV_ENVELOPE",
            "ZIRV_CTX_SESSION",
            "GIT_DIR",
        ] {
            assert!(!scrubbed.contains_key(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn setup_steps_are_killed_at_their_deadline() {
        if cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = workspace();
        config.setup.push("sleep 30".into());
        let started = Instant::now();
        let error = run_setup_with_timeout(&config, temp.path(), Duration::from_millis(50))
            .expect_err("setup must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn clone_destination_rejects_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        symlink(&outside, root.join("deps")).expect("symlink");

        let error = secure_destination(&workspace(), &root, Path::new("deps/repo"))
            .expect_err("symlink ancestor must be rejected");
        assert!(error.to_string().contains("is a symlink"), "{error}");
        assert!(!outside.join("repo").exists());
    }

    #[test]
    fn extra_repositories_are_cloned_before_ordered_setup_runs() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .map_or(true, |status| !status.success())
            || cfg!(windows)
        {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let root = temp.path().join("workspace");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::create_dir_all(&root).expect("root");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(args)
                .status()
                .expect("git")
                .success()
        };
        assert!(git(&["init", "-q"]));
        assert!(git(&["config", "user.email", "test@example.com"]));
        assert!(git(&["config", "user.name", "Test"]));
        assert!(git(&["branch", "-M", "main"]));
        std::fs::write(source.join("README.md"), "dependency\n").expect("readme");
        assert!(git(&["add", "README.md"]));
        assert!(git(&["commit", "-q", "-m", "initial"]));

        let mut config = workspace();
        config.git.push(WorkspaceGit {
            repo: source.display().to_string(),
            branch: "main".into(),
            dir: PathBuf::from("deps/docs"),
        });
        config
            .setup
            .push("test -f deps/docs/README.md && touch ready".into());

        clone_repositories(&config, &root).expect("clone");
        run_setup(&config, &root).expect("setup after clone");
        assert!(root.join("deps/docs/README.md").is_file());
        assert!(root.join("ready").is_file());
        clone_repositories(&config, &root).expect("existing matching clone is idempotent");
    }
}
