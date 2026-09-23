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
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::adapters::AgentAdapter;
use super::config::EnvLookup;
use super::state::{StateDir, create_private_dir_all, open_private_append};
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
    for requested in &workspace.skills {
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
    attach_skill_refs(&workspace.name, &workspace.skills, repo, prompt, env)
}

/// Attach an already-authorized list of skill references through the one
/// workspace renderer. Agent-manifest defaults use this too, so dependency
/// resolution and the untrusted-instruction label cannot drift.
pub fn attach_skill_refs(
    source: &str,
    requested_skills: &[SkillRef],
    repo: &Path,
    prompt: String,
    env: EnvLookup<'_>,
) -> CtxResult<String> {
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
            .map_err(|error| format!("skill source '{source}': {error}"))?;
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
    state: &StateDir,
    root: &Path,
    adapter: &dyn AgentAdapter,
    adapter_flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<WorkspaceReady> {
    validate_mcp_servers(workspace, root, adapter, adapter_flags, env)?;
    clone_repositories(workspace, root)?;
    run_setup(workspace, state, root)?;
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
        let outcome = run_bounded(&mut command, CLONE_TIMEOUT, &[("GIT_TERMINAL_PROMPT", "0")])
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
            supervise::Outcome::StoppedByTick(_) => {
                unreachable!("workspace commands never stop by tick")
            }
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
    let git_metadata = destination.join(".git");
    if std::fs::symlink_metadata(&git_metadata)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!(
            "workspace '{}': refusing git dir '{}' with symlinked .git metadata",
            workspace.name,
            git.dir.display()
        )
        .into());
    }
    let top_level = git_output(destination, &["rev-parse", "--show-toplevel"])?;
    let canonical_destination = std::fs::canonicalize(destination)?;
    let canonical_top_level = std::fs::canonicalize(top_level)?;
    if canonical_top_level != canonical_destination {
        return Err(format!(
            "workspace '{}': existing git dir '{}' is not a repository root",
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

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SetupProgress {
    step_index: usize,
    step_digest: String,
    completed_at: u64,
}

fn setup_record_path(state: &StateDir, root: &Path) -> PathBuf {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if canonical
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "worktrees")
        && canonical
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == crate::utils::SCRIPT_DIR_NAME)
        && let (Some(source_repo), Some(short)) = (
            canonical
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent),
            canonical.file_name().and_then(|name| name.to_str()),
        )
    {
        return state
            .worktrees()
            .join(super::state::repo_slug(source_repo))
            .join(format!("{short}-setup.jsonl"));
    }
    let rendered = canonical.to_string_lossy();
    let digest = Sha256::digest(rendered.as_bytes());
    let suffix: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let stem = canonical
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "checkout".to_string());
    state
        .worktrees()
        .join(super::state::repo_slug(&canonical))
        .join(format!("{stem}-{suffix}-setup.jsonl"))
}

fn setup_step_digest(command: &str) -> String {
    Sha256::digest(command.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn completed_setup_steps(path: &Path) -> HashSet<(usize, String)> {
    std::fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|contents| contents.lines().map(str::to_owned).collect::<Vec<_>>())
        .filter_map(|line| serde_json::from_str::<SetupProgress>(&line).ok())
        .map(|record| (record.step_index, record.step_digest))
        .collect()
}

fn append_setup_completion(path: &Path, step_index: usize, step_digest: String) -> CtxResult<()> {
    let parent = path.parent().ok_or("setup progress path has no parent")?;
    create_private_dir_all(parent)?;
    let mut file = open_private_append(path)?;
    // The leading newline turns a torn final record into one ignorable row,
    // keeping this success record independently parseable on the next read.
    writeln!(
        file,
        "\n{}",
        serde_json::to_string(&SetupProgress {
            step_index,
            step_digest,
            completed_at: super::state::now_secs(),
        })?
    )?;
    Ok(())
}

fn run_setup(workspace: &WorkspaceConfig, state: &StateDir, root: &Path) -> CtxResult<()> {
    run_setup_with_timeout(workspace, state, root, SETUP_STEP_TIMEOUT)
}

fn run_setup_with_timeout(
    workspace: &WorkspaceConfig,
    state: &StateDir,
    root: &Path,
    timeout: Duration,
) -> CtxResult<()> {
    let path = setup_record_path(state, root);
    let completed = completed_setup_steps(&path);
    for (index, command) in workspace.setup.iter().enumerate() {
        let digest = setup_step_digest(command);
        if completed.contains(&(index, digest.clone())) {
            continue;
        }
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
            supervise::Outcome::Exited(0) => append_setup_completion(&path, index, digest)?,
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
            supervise::Outcome::StoppedByTick(_) => {
                unreachable!("workspace commands never stop by tick")
            }
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

fn claude_mcp_servers(
    repo: &Path,
    flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<BTreeSet<String>> {
    let mut servers: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut disabled = BTreeSet::new();
    let mut project_servers = BTreeSet::new();
    let strict = flags.iter().any(|flag| flag == "--strict-mcp-config");
    if flags
        .iter()
        .any(|flag| flag == "--plugin-dir" || flag.starts_with("--plugin-dir="))
    {
        return Err("adapter 'claude': MCP servers contributed by --plugin-dir cannot be resolved safely; refusing workspace MCP validation".into());
    }
    let (user_state, settings_dir) = if let Some(dir) = env("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
    {
        (Some(dir.join(".claude.json")), Some(dir))
    } else if let Some(home) = home_dir(env) {
        (Some(home.join(".claude.json")), Some(home.join(".claude")))
    } else {
        (None, None)
    };
    if !strict {
        if let Some(path) = user_state.as_deref() {
            let state = read_optional_json(path)?;
            if let Some(value) = state.as_ref() {
                replace_json_servers(value.get("mcpServers"), &mut servers);
                collect_json_names(value.get("disabledMcpServers"), &mut disabled);
            }
        }
        let project = read_optional_json(&repo.join(".mcp.json"))?;
        if let Some(value) = project.as_ref() {
            replace_json_servers(value.get("mcpServers"), &mut servers);
            if let Some(configured) = value
                .get("mcpServers")
                .and_then(serde_json::Value::as_object)
            {
                project_servers.extend(configured.keys().cloned());
            }
        }
        if let Some(path) = user_state.as_deref()
            && let Some(value) = read_optional_json(path)?
            && let Some(project) = exact_claude_project(&value, repo)
        {
            replace_json_servers(project.get("mcpServers"), &mut servers);
            collect_json_names(project.get("disabledMcpServers"), &mut disabled);
        }
    }
    for value in flag_values(flags, "--mcp-config") {
        let config = if value.trim_start().starts_with('{') {
            serde_json::from_str(value).map_err(|error| {
                format!("adapter 'claude': invalid inline --mcp-config JSON: {error}")
            })?
        } else {
            let path = resolve_config_path(repo, value);
            read_required_json(&path)?
        };
        replace_json_servers(config.get("mcpServers"), &mut servers);
    }

    let mut disabled_project_servers = BTreeSet::new();
    collect_claude_settings_disables(
        repo,
        flags,
        settings_dir.as_deref(),
        &mut disabled_project_servers,
    )?;
    for name in project_servers.intersection(&disabled_project_servers) {
        servers.remove(name);
    }
    for name in disabled {
        servers.remove(&name);
    }
    Ok(valid_json_mcp_names(&servers))
}

fn codex_mcp_servers(
    repo: &Path,
    flags: &[String],
    env: EnvLookup<'_>,
) -> CtxResult<BTreeSet<String>> {
    let mut effective = toml::Table::new();
    let mut trusted_root = None;
    if !flags.iter().any(|flag| flag == "--ignore-user-config")
        && let Some(home) = env("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir(env).map(|h| h.join(".codex")))
        && let Some(user) = read_optional_toml(&home.join("config.toml"))?
    {
        trusted_root = codex_trusted_root(&user, repo);
        merge_toml_table(&mut effective, user);
    }
    if let Some(root) = trusted_root {
        for path in codex_project_config_paths(&root, repo)? {
            if let Some(project) = read_optional_toml(&path)? {
                merge_toml_table(&mut effective, project);
            }
        }
    }

    if effective.contains_key("profile") {
        return Err("adapter 'codex': profile-based MCP configuration cannot be resolved safely; refusing workspace MCP validation".into());
    }

    let mut index = 0;
    while index < flags.len() {
        let value = if flags[index] == "-c" || flags[index] == "--config" {
            index += 1;
            flags.get(index).map(String::as_str)
        } else {
            flags[index]
                .strip_prefix("--config=")
                .or_else(|| flags[index].strip_prefix("-c="))
                .or_else(|| {
                    flags[index]
                        .strip_prefix("-c")
                        .filter(|value| !value.is_empty())
                })
        };
        if let Some(value) = value {
            match assignment_root(value).as_deref() {
                Some("mcp_servers") => apply_toml_assignment(&mut effective, value)?,
                Some("profile") => {
                    return Err("adapter 'codex': profile-based MCP configuration cannot be resolved safely; refusing workspace MCP validation".into());
                }
                _ => {}
            }
        }
        index += 1;
    }
    if flags.iter().any(|flag| {
        matches!(flag.as_str(), "--profile" | "-p")
            || flag.starts_with("--profile=")
            || flag.starts_with("-p=")
            || (flag.starts_with("-p") && flag.len() > 2)
    }) {
        return Err("adapter 'codex': --profile may change MCP configuration; refusing workspace MCP validation".into());
    }
    Ok(valid_toml_mcp_names(&effective))
}

fn read_optional_json(path: &Path) -> CtxResult<Option<serde_json::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    read_required_json(path).map(Some)
}

fn read_required_json(path: &Path) -> CtxResult<serde_json::Value> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read MCP config '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid MCP config '{}': {error}", path.display()).into())
}

fn exact_claude_project<'a>(
    value: &'a serde_json::Value,
    repo: &Path,
) -> Option<&'a serde_json::Value> {
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    value
        .get("projects")?
        .as_object()?
        .iter()
        .find(|(path, _)| {
            PathBuf::from(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path))
                == canonical
        })
        .map(|(_, project)| project)
}

fn replace_json_servers(
    configured: Option<&serde_json::Value>,
    servers: &mut BTreeMap<String, serde_json::Value>,
) {
    if let Some(configured) = configured.and_then(serde_json::Value::as_object) {
        for (name, config) in configured {
            servers.insert(name.clone(), config.clone());
        }
    }
}

fn collect_json_names(value: Option<&serde_json::Value>, names: &mut BTreeSet<String>) {
    if let Some(values) = value.and_then(serde_json::Value::as_array) {
        names.extend(
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned),
        );
    }
}

fn collect_claude_settings_disables(
    repo: &Path,
    flags: &[String],
    config_dir: Option<&Path>,
    disabled_project_servers: &mut BTreeSet<String>,
) -> CtxResult<()> {
    let selected_sources = flag_values(flags, "--setting-sources")
        .last()
        .map(|raw| raw.split(',').map(str::trim).collect::<BTreeSet<_>>());
    let includes = |source: &str| {
        selected_sources
            .as_ref()
            .is_none_or(|sources| sources.contains(source))
    };
    let mut paths = Vec::new();
    if includes("user")
        && let Some(dir) = config_dir
    {
        paths.push(dir.join("settings.json"));
        paths.push(dir.join("settings.local.json"));
    }
    if includes("project") {
        paths.push(repo.join(".claude/settings.json"));
    }
    if includes("local") {
        paths.push(repo.join(".claude/settings.local.json"));
    }
    for path in paths {
        if let Some(value) = read_optional_json(&path)? {
            collect_json_names(
                value.get("disabledMcpjsonServers"),
                disabled_project_servers,
            );
        }
    }
    for raw in flag_values(flags, "--settings") {
        let value = if raw.trim_start().starts_with('{') {
            serde_json::from_str(raw).map_err(|error| {
                format!("adapter 'claude': invalid inline --settings JSON: {error}")
            })?
        } else {
            let path = resolve_config_path(repo, raw);
            if cfg!(test)
                && path.file_name().and_then(|name| name.to_str())
                    == Some("zirv-test-claude-launch-settings.json")
                && !path.exists()
            {
                continue;
            }
            read_required_json(&path)?
        };
        collect_json_names(
            value.get("disabledMcpjsonServers"),
            disabled_project_servers,
        );
    }
    Ok(())
}

fn valid_json_mcp_names(servers: &BTreeMap<String, serde_json::Value>) -> BTreeSet<String> {
    servers
        .iter()
        .filter(|(_, config)| config.as_object().is_some_and(valid_server_fields_json))
        .map(|(name, _)| name.clone())
        .collect()
}

fn valid_server_fields_json(entry: &serde_json::Map<String, serde_json::Value>) -> bool {
    entry
        .get("command")
        .or_else(|| entry.get("url"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn read_optional_toml(path: &Path) -> CtxResult<Option<toml::Table>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read MCP config '{}': {error}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|error| format!("invalid MCP config '{}': {error}", path.display()))?;
    Ok(Some(table))
}

fn codex_trusted_root(user: &toml::Table, repo: &Path) -> Option<PathBuf> {
    let canonical_repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    user.get("projects")?
        .as_table()?
        .iter()
        .filter_map(|(path, config)| {
            let candidate = PathBuf::from(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path));
            canonical_repo.starts_with(&candidate).then(|| {
                let trusted = config
                    .as_table()
                    .and_then(|table| table.get("trust_level"))
                    .and_then(toml::Value::as_str)
                    == Some("trusted");
                (candidate.components().count(), candidate, trusted)
            })
        })
        .max_by_key(|(depth, _, _)| *depth)
        .and_then(|(_, path, trusted)| trusted.then_some(path))
}

fn codex_project_config_paths(root: &Path, repo: &Path) -> CtxResult<Vec<PathBuf>> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let relative = repo.strip_prefix(root).map_err(|_| {
        format!(
            "adapter 'codex': trusted project root '{}' is not an ancestor of '{}'",
            root.display(),
            repo.display()
        )
    })?;
    let mut current = root.to_path_buf();
    let mut paths = vec![current.join(".codex/config.toml")];
    for component in relative.components() {
        current.push(component.as_os_str());
        paths.push(current.join(".codex/config.toml"));
    }
    Ok(paths)
}

fn merge_toml_table(target: &mut toml::Table, source: toml::Table) {
    for (key, value) in source {
        match (target.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge_toml_table(existing, incoming);
            }
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
}

fn assignment_root(raw: &str) -> Option<String> {
    let (key, _) = raw.split_once('=')?;
    let probe: toml::Table = format!("{} = true", key.trim()).parse().ok()?;
    probe.keys().next().cloned()
}

fn apply_toml_assignment(target: &mut toml::Table, raw: &str) -> CtxResult<()> {
    let (key, _) = raw
        .split_once('=')
        .ok_or_else(|| format!("adapter 'codex': invalid config override '{raw}'"))?;
    let parsed: toml::Table = raw
        .parse()
        .map_err(|error| format!("adapter 'codex': invalid MCP config override: {error}"))?;
    let path = toml_assignment_path(key.trim())?;
    let value = toml_value_at_path(&parsed, &path)
        .cloned()
        .ok_or_else(|| format!("adapter 'codex': invalid MCP config override '{raw}'"))?;
    set_toml_value(target, &path, value);
    Ok(())
}

fn toml_assignment_path(key: &str) -> CtxResult<Vec<String>> {
    let probe: toml::Table = format!("{key} = true")
        .parse()
        .map_err(|error| format!("adapter 'codex': invalid MCP config override key: {error}"))?;
    let mut path = Vec::new();
    let mut table = &probe;
    loop {
        if table.len() != 1 {
            return Err("adapter 'codex': config override must assign one key".into());
        }
        let (key, value) = table.iter().next().expect("one entry checked");
        path.push(key.clone());
        match value {
            toml::Value::Table(next) => table = next,
            _ => return Ok(path),
        }
    }
}

fn toml_value_at_path<'a>(table: &'a toml::Table, path: &[String]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let value = table.get(first)?;
    if rest.is_empty() {
        Some(value)
    } else {
        toml_value_at_path(value.as_table()?, rest)
    }
}

fn set_toml_value(table: &mut toml::Table, path: &[String], value: toml::Value) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert(first.clone(), value);
        return;
    }
    let entry = table
        .entry(first.clone())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    set_toml_value(entry.as_table_mut().expect("table set above"), rest, value);
}

fn valid_toml_mcp_names(config: &toml::Table) -> BTreeSet<String> {
    config
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(toml::Table::iter)
        .filter(|(_, config)| {
            config.as_table().is_some_and(|entry| {
                entry.get("enabled").and_then(toml::Value::as_bool) != Some(false)
                    && entry
                        .get("command")
                        .or_else(|| entry.get("url"))
                        .and_then(toml::Value::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
            })
        })
        .map(|(name, _)| name.clone())
        .collect()
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
    fn codex_mcp_leaf_overrides_preserve_disabled_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).expect("mkdir");
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'old'\nenabled = false\n\n[mcp_servers.linear]\ncommand = 'linear'\n",
        )
        .expect("config");
        let env = |key: &str| (key == "HOME").then(|| home.display().to_string());

        let names = codex_mcp_servers(
            temp.path(),
            &["-c".into(), "mcp_servers.docs.command='new'".into()],
            &env,
        )
        .expect("discover");
        assert_eq!(names, BTreeSet::from(["linear".into()]));

        let names = codex_mcp_servers(
            temp.path(),
            &["-c".into(), "mcp_servers.linear.enabled=false".into()],
            &env,
        )
        .expect("discover");
        assert!(names.is_empty());

        let names = codex_mcp_servers(
            temp.path(),
            &[
                "-c".into(),
                "mcp_servers.docs={command='new'}".into(),
                "-c".into(),
                "mcp_servers.\"docs.api\"={command='new'}".into(),
            ],
            &env,
        )
        .expect("whole server assignment");
        assert_eq!(
            names,
            BTreeSet::from(["docs".into(), "docs.api".into(), "linear".into()])
        );
    }

    #[test]
    fn codex_mcp_whole_map_override_removes_all_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).expect("mkdir");
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'docs'\n",
        )
        .expect("config");
        let names = codex_mcp_servers(temp.path(), &["-cmcp_servers={}".into()], &|key| {
            (key == "HOME").then(|| home.display().to_string())
        })
        .expect("discover");
        assert!(names.is_empty());
    }

    #[test]
    fn codex_project_mcp_requires_operator_trust_and_obeys_closest_trust() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let root = temp.path().join("root");
        let repo = root.join("nested");
        std::fs::create_dir_all(home.join(".codex")).expect("home");
        std::fs::create_dir_all(repo.join(".codex")).expect("repo");
        std::fs::write(
            repo.join(".codex/config.toml"),
            "[mcp_servers.home]\nenabled = false\n\n[mcp_servers.project]\ncommand = 'project'\n",
        )
        .expect("project config");
        let user = |trust: &str| {
            format!(
                "[mcp_servers.home]\ncommand = 'home'\n\n[projects.{}]\ntrust_level = '{trust}'\n",
                toml::Value::String(root.display().to_string())
            )
        };
        std::fs::write(home.join(".codex/config.toml"), user("trusted")).expect("user config");
        let env = |key: &str| (key == "HOME").then(|| home.display().to_string());
        assert_eq!(
            codex_mcp_servers(&repo, &[], &env).expect("trusted discovery"),
            BTreeSet::from(["project".into()])
        );

        std::fs::write(
            home.join(".codex/config.toml"),
            format!(
                "{}\n[projects.{}]\ntrust_level = 'untrusted'\n",
                user("trusted"),
                toml::Value::String(repo.display().to_string())
            ),
        )
        .expect("closer trust");
        assert_eq!(
            codex_mcp_servers(&repo, &[], &env).expect("untrusted discovery"),
            BTreeSet::from(["home".into()])
        );

        std::fs::write(home.join(".codex/config.toml"), "").expect("no operator trust");
        std::fs::write(
            repo.join(".codex/config.toml"),
            format!(
                "[projects.{}]\ntrust_level = 'trusted'\n\n[mcp_servers.project]\ncommand = 'project'\n",
                toml::Value::String(repo.display().to_string())
            ),
        )
        .expect("self-authorizing project config");
        assert!(
            codex_mcp_servers(&repo, &[], &env)
                .expect("project cannot authorize itself")
                .is_empty()
        );
    }

    #[test]
    fn claude_project_disable_survives_explicit_server_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join("claude");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&repo).expect("repo");
        let state = serde_json::json!({
            "mcpServers": {"docs": {"command": "user-docs"}},
            "projects": {
                repo.display().to_string(): {
                    "disabledMcpServers": ["docs"]
                }
            }
        });
        std::fs::write(
            config_dir.join(".claude.json"),
            serde_json::to_vec(&state).expect("json"),
        )
        .expect("state");
        let names = claude_mcp_servers(
            &repo,
            &[
                "--mcp-config".into(),
                r#"{"mcpServers":{"docs":{"command":"explicit-docs"}}}"#.into(),
            ],
            &|key| (key == "CLAUDE_CONFIG_DIR").then(|| config_dir.display().to_string()),
        )
        .expect("discover");
        assert!(names.is_empty());
    }

    #[test]
    fn claude_strict_and_project_mcp_sources_match_headless_launch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join("claude");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(repo.join(".claude")).expect("settings dir");
        std::fs::write(repo.join(".mcp.json"), "not json").expect("malformed project config");
        let env =
            |key: &str| (key == "CLAUDE_CONFIG_DIR").then(|| config_dir.display().to_string());
        let strict = claude_mcp_servers(
            &repo,
            &[
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                r#"{"mcpServers":{"explicit":{"command":"explicit"}}}"#.into(),
            ],
            &env,
        )
        .expect("strict ignores ordinary project config");
        assert_eq!(strict, BTreeSet::from(["explicit".into()]));

        std::fs::write(
            repo.join(".mcp.json"),
            r#"{"mcpServers":{"project":{"command":"project"}}}"#,
        )
        .expect("project config");
        assert_eq!(
            claude_mcp_servers(&repo, &[], &env).expect("ordinary project config"),
            BTreeSet::from(["project".into()])
        );
        std::fs::write(
            repo.join(".claude/settings.json"),
            r#"{"disabledMcpjsonServers":["project"]}"#,
        )
        .expect("project settings");
        assert!(
            claude_mcp_servers(&repo, &[], &env)
                .expect("project MCP disabled")
                .is_empty()
        );
    }

    #[test]
    fn failed_setup_stops_before_later_steps() {
        if cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(temp.path().join("state"));
        let mut config = workspace();
        config.setup = vec!["false".into(), "touch should-not-exist".into()];
        let error = run_setup(&config, &state, temp.path()).expect_err("first step fails");
        assert!(error.to_string().contains("setup step 1 failed"));
        assert!(!temp.path().join("should-not-exist").exists());
    }

    #[test]
    fn workspace_subprocess_environment_excludes_credentials_and_zirv_authority() {
        let environment = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
            (
                OsString::from("CUSTOM_CREDENTIAL"),
                OsString::from("secret"),
            ),
            (OsString::from("ZIRV_ENVELOPE"), OsString::from("authority")),
            (
                OsString::from("ZIRV_CTX_SESSION"),
                OsString::from("session"),
            ),
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
        let state = StateDir::from_root(temp.path().join("state"));
        let mut config = workspace();
        config.setup.push("sleep 30".into());
        let started = Instant::now();
        let error = run_setup_with_timeout(&config, &state, temp.path(), Duration::from_millis(50))
            .expect_err("setup must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn setup_progress_resumes_only_matching_successes_and_ignores_torn_rows() {
        if cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(temp.path().join("state"));
        let counter = temp.path().join("counter");
        let later = temp.path().join("later");
        let mut config = workspace();
        config.setup = vec![
            format!("printf first >> {}", counter.display()),
            "false".into(),
            format!("touch {}", later.display()),
        ];
        run_setup(&config, &state, temp.path()).expect_err("partial setup fails");
        assert_eq!(
            std::fs::read_to_string(&counter).expect("first ran"),
            "first"
        );
        assert!(!later.exists());

        let path = setup_record_path(&state, temp.path());
        let mut progress = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("progress exists");
        write!(progress, "{{\"torn\"").expect("torn row");
        config.setup[1] = "true".into();
        run_setup(&config, &state, temp.path()).expect("retry succeeds");
        assert_eq!(
            std::fs::read_to_string(&counter).expect("first remains once"),
            "first"
        );
        assert!(later.exists());

        config.setup[0] = format!("printf changed >> {}", counter.display());
        run_setup(&config, &state, temp.path()).expect("changed command reruns");
        assert_eq!(
            std::fs::read_to_string(&counter).expect("counter"),
            "firstchanged"
        );
    }

    #[test]
    fn setup_progress_identity_is_root_and_index_sensitive() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(temp.path().join("state"));
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        std::fs::create_dir_all(&left).expect("left");
        std::fs::create_dir_all(&right).expect("right");
        assert_ne!(
            setup_record_path(&state, &left),
            setup_record_path(&state, &right)
        );

        let digest = setup_step_digest("echo setup");
        let completed = HashSet::from([(0, digest.clone())]);
        assert!(completed.contains(&(0, digest.clone())));
        assert!(!completed.contains(&(1, digest)));
    }

    #[test]
    fn setup_step_digest_matches_sha256_vector() {
        assert_eq!(
            setup_step_digest("echo setup"),
            "893a2fc5244216ce6b1c0796aa0a30dd959fdb7ca95256aed58892a08fc8174c"
        );
    }

    #[test]
    fn managed_worktree_progress_uses_source_repo_slug_and_short_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let checkout = source
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join("worktrees")
            .join("abcd1234");
        std::fs::create_dir_all(&checkout).expect("checkout");
        let state = StateDir::from_root(temp.path().join("state"));

        assert_eq!(
            setup_record_path(&state, &checkout),
            state
                .worktrees()
                .join(super::super::state::repo_slug(&source))
                .join("abcd1234-setup.jsonl")
        );
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
        let state = StateDir::from_root(temp.path().join("state"));
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
        run_setup(&config, &state, &root).expect("setup after clone");
        assert!(root.join("deps/docs/README.md").is_file());
        assert!(root.join("ready").is_file());
        clone_repositories(&config, &root).expect("existing matching clone is idempotent");
    }

    #[test]
    fn existing_clone_must_be_its_own_repository_root() {
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
        for path in [&source, &root] {
            std::fs::create_dir_all(path).expect("mkdir");
            let git = |args: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(path)
                    .args(args)
                    .status()
                    .expect("git")
                    .success()
            };
            assert!(git(&["init", "-q"]));
            assert!(git(&["config", "user.email", "test@example.com"]));
            assert!(git(&["config", "user.name", "Test"]));
            assert!(git(&["branch", "-M", "main"]));
            std::fs::write(path.join("README.md"), "x").expect("write");
            assert!(git(&["add", "README.md"]));
            assert!(git(&["commit", "-q", "-m", "initial"]));
        }
        let source_text = source.display().to_string();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["remote", "add", "origin", &source_text])
                .status()
                .expect("remote")
                .success()
        );
        std::fs::create_dir(root.join("deps")).expect("ordinary directory");
        let git = WorkspaceGit {
            repo: source_text,
            branch: "main".into(),
            dir: PathBuf::from("deps"),
        };
        let error = validate_existing_clone(&workspace(), &git, &root.join("deps"))
            .expect_err("a child directory must not inherit its parent's git repository");
        assert!(
            error.to_string().contains("not a repository root"),
            "{error}"
        );
    }
}
