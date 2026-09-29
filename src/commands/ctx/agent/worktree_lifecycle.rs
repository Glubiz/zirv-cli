//! Allocation, reuse and reclaim of a delegated worker's own worktree/workdir,
//! plus the out-of-repo path warnings that use the same launch-repo notion.

use std::path::{Path, PathBuf};

use super::super::CtxResult;
use super::super::adapters;
use super::super::config::EnvLookup;
use super::super::event::SessionId;
use super::super::permit::WorkerMode;
use super::super::state::StateDir;
use super::super::worktree;
use super::*;

/// Require a canonical existing git workdir before deriving its sandbox; no non-repo escape hatch (#228).
/// The dashboard must rerun this check because a same-uid spawn request is untrusted data (#179).
pub(crate) fn validate_workdir(dir: &Path) -> CtxResult<PathBuf> {
    let canon = std::fs::canonicalize(dir)
        .map_err(|e| format!("--workdir {} does not exist: {e}", dir.display()))?;
    let home = crate::utils::home_dir()?;
    let state = StateDir::resolve(&super::super::config::env_from_process())?;
    let mut homes = WorkdirHomes::new(&home, state.root(), &|key| std::env::var(key).ok());
    homes.home = std::fs::canonicalize(&homes.home).unwrap_or(homes.home);
    for (_, root) in &mut homes.roots {
        if let Some(canonical) = super::super::pathutil::canonicalize_with_missing_tail(root) {
            *root = canonical;
        }
    }
    if let Some(root) = refused_workdir_root(&canon, &homes) {
        return Err(format!(
            "--workdir {} is refused: protected root {root}",
            canon.display()
        )
        .into());
    }
    if !canon.is_dir() {
        return Err(format!("--workdir {} is not a directory", dir.display()).into());
    }
    if adapters::git_common_dir(&canon).is_none() {
        return Err(format!(
            "--workdir {} is not inside a git repository; zirv agent workers need a repository \
             checkout",
            dir.display()
        )
        .into());
    }
    Ok(canon)
}

struct WorkdirHomes {
    home: PathBuf,
    roots: Vec<(&'static str, PathBuf)>,
}

impl WorkdirHomes {
    fn new(home: &Path, state: &Path, env: super::super::config::EnvLookup<'_>) -> Self {
        let configured = |key, fallback: PathBuf| {
            env(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or(fallback)
        };
        Self {
            home: home.to_path_buf(),
            roots: vec![
                ("~/.zirv", home.join(".zirv")),
                ("zirv state directory", state.to_path_buf()),
                (
                    "CLAUDE_CONFIG_DIR or ~/.claude",
                    configured("CLAUDE_CONFIG_DIR", home.join(".claude")),
                ),
                (
                    "CODEX_HOME or ~/.codex",
                    configured("CODEX_HOME", home.join(".codex")),
                ),
                (
                    "COPILOT_HOME or ~/.copilot",
                    configured("COPILOT_HOME", home.join(".copilot")),
                ),
                ("~/.factory", home.join(".factory")),
                ("~/.gemini", home.join(".gemini")),
                (
                    "XDG_DATA_HOME/opencode or ~/.local/share/opencode",
                    configured("XDG_DATA_HOME", home.join(".local/share")).join("opencode"),
                ),
                (
                    "PI_CODING_AGENT_DIR or ~/.pi/agent",
                    configured("PI_CODING_AGENT_DIR", home.join(".pi/agent")),
                ),
                (
                    "QWEN_HOME or ~/.qwen",
                    configured("QWEN_HOME", home.join(".qwen")),
                ),
                (
                    "QWEN_RUNTIME_DIR or ~/.qwen",
                    configured(
                        "QWEN_RUNTIME_DIR",
                        configured("QWEN_HOME", home.join(".qwen")),
                    ),
                ),
                ("~/.ssh", home.join(".ssh")),
            ],
        }
    }
}

/// Refuse filesystem roots/home by equality and config/state roots with descendants; no flag overrides this.
fn refused_workdir_root(canonical: &Path, homes: &WorkdirHomes) -> Option<&'static str> {
    if canonical.has_root() && canonical.parent().is_none() {
        return Some("filesystem root");
    }
    if canonical == homes.home {
        return Some("$HOME");
    }
    homes
        .roots
        .iter()
        .find_map(|(name, root)| canonical.starts_with(root).then_some(*name))
}

/// Under the worktree lock, claim Idle as Active before reset so concurrent callers cannot reuse the same tree (#718).
/// Reset or validation failure marks InspectionFailed and falls back to cold allocation, never leaves a half-reset tree Idle.
fn claim_idle_worktree(
    state: &StateDir,
    repo_slug: &str,
    digest: &str,
    base_commit: &str,
    owner_session: Option<&str>,
) -> Option<PathBuf> {
    let reusable = worktree::find_reusable(state, repo_slug, digest)?;
    let path = PathBuf::from(&reusable.path);
    let claimed = worktree::WorktreeRecord {
        path: path.to_string_lossy().to_string(),
        branch: reusable.branch,
        base_commit: base_commit.to_string(),
        owner_session: owner_session.map(str::to_string),
        owner_pid: Some(std::process::id()),
        created_at: super::super::state::now_secs(),
        status: worktree::WorktreeStatus::Active,
        note: None,
        setup_digest: Some(digest.to_string()),
        idled_at: None,
    };
    if let Err(e) = worktree::append_record(state, repo_slug, &claimed) {
        eprintln!(
            "--worktree {}: could not record reuse ownership ({e}); allocating a fresh one \
             instead",
            path.display()
        );
        return None;
    }
    let reset_ok = worktree::git_command(&path)
        .arg("reset")
        .arg("--hard")
        .arg(base_commit)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if reset_ok && let Ok(validated) = validate_workdir(&path) {
        return Some(validated);
    }
    eprintln!(
        "--worktree {}: could not reuse the idle tree (reset or validation failed after the \
         claim); marking it for manual inspection and allocating a fresh one instead",
        path.display()
    );
    let _ = worktree::update_status(
        state,
        repo_slug,
        &path,
        worktree::WorktreeStatus::InspectionFailed,
        Some("git reset --hard failed after a --worktree-reuse claim".to_string()),
    );
    None
}

/// Capture and pass the base commit before worktree creation so later removal has an exact ownership proof (#267, #319).
/// Validate the allocated git directory; allocation failure must refuse, never fall back to the shared checkout.
/// Ownership recording is best-effort: missing records prevent reclamation, not allocation.
pub(super) fn allocate_worktree(
    state: &StateDir,
    repo: &Path,
    owner_session: Option<&str>,
    reuse: bool,
    setup: &[String],
) -> CtxResult<PathBuf> {
    let base_commit = worktree::git_command(repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .map_err(|e| format!("--worktree: could not run `git rev-parse HEAD`: {e}"))?;
    if !base_commit.status.success() {
        return Err(format!(
            "--worktree: `git rev-parse HEAD` failed: {}",
            String::from_utf8_lossy(&base_commit.stderr).trim()
        )
        .into());
    }
    let base_commit = String::from_utf8_lossy(&base_commit.stdout)
        .trim()
        .to_string();
    let repo_slug = super::super::state::repo_slug(repo);
    // Reuse only a re-proved clean tree under the worktree lock; lock/reset/validation failure uses cold allocation (#718).
    // JSON-encode ordered setup commands in the digest so command boundaries, removals and reordering cannot collide.
    let setup = if setup.is_empty() {
        String::new()
    } else {
        serde_json::to_string(setup)?
    };
    let digest = reuse.then(|| worktree::setup_digest(&base_commit, &setup));
    if let Some(digest) = &digest {
        match worktree::lock_worktrees(state, &repo_slug) {
            Ok(_lock) => {
                if let Some(path) =
                    claim_idle_worktree(state, &repo_slug, digest, &base_commit, owner_session)
                {
                    return Ok(path);
                }
            }
            Err(e) => {
                eprintln!(
                    "--worktree-reuse: could not lock the worktree store ({e}); skipping reuse \
                     for this allocation"
                );
            }
        }
    }

    let root = repo.join(crate::utils::SCRIPT_DIR_NAME).join("worktrees");
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("--worktree: could not create {}: {e}", root.display()))?;
    let short = super::super::sessions::short_id(&SessionId::new_v4().to_string());
    let path = root.join(&short);
    let output = worktree::git_command(repo)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&short)
        .arg(&path)
        .arg(&base_commit)
        .output()
        .map_err(|e| format!("--worktree: could not run `git worktree add`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "--worktree: `git worktree add {}` failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let path = validate_workdir(&path)?;
    let record = worktree::WorktreeRecord {
        path: path.to_string_lossy().to_string(),
        branch: short,
        base_commit,
        owner_session: owner_session.map(str::to_string),
        owner_pid: Some(std::process::id()),
        created_at: super::super::state::now_secs(),
        status: worktree::WorktreeStatus::Active,
        note: None,
        setup_digest: digest,
        idled_at: None,
    };
    if let Err(e) = worktree::append_record(state, &repo_slug, &record) {
        eprintln!(
            "--worktree {}: could not record ownership ({e}); a later reclaim will require \
             manual `zirv ctx worktree prune`",
            path.display()
        );
    }
    Ok(path)
}

/// Reclaim failures are best-effort diagnostics, never delegation failures; dashboard exits share these outcomes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReclaimOutcome {
    /// Proved safe and removed without force; the branch survives, with only regenerable ignored output allowed.
    Removed,
    /// Untracked or ignored content was archived before removal.
    Archived(PathBuf),
    /// Missing ownership, unsafe content or failed probes leave the tree in place with the failed proof (#319).
    InspectionFailed { probe: &'static str, note: String },
    /// Failed archive or removal leaves the tree in place.
    Failed(String),
    /// Opt-in reuse passed removal proofs and pool admission; retain the tree and warm cache as Idle (#718).
    Idled,
}

/// All reclaim paths share `prune_one`'s proof requirement; missing ownership means keep the tree (#319).
/// Reuse idling also requires that proof and a locked capacity check, preventing races with allocation (#718).
/// A full pool, Keep refusal or lock failure falls through to ordinary proof-required reclamation.
pub(crate) fn reclaim_worktree(
    state: &StateDir,
    repo: &Path,
    path: &Path,
    idle_pool_max: u32,
) -> ReclaimOutcome {
    let repo_slug = super::super::state::repo_slug(repo);
    let Some(record) = worktree::latest_for_path(state, &repo_slug, path) else {
        return ReclaimOutcome::InspectionFailed {
            probe: "record",
            note: "no ownership record for this worktree; run `zirv ctx worktree prune` after \
                   manual inspection"
                .to_string(),
        };
    };
    if record.setup_digest.is_some() {
        // Use the allocation lock so idling and concurrent claims cannot race the pool count.
        match worktree::lock_worktrees(state, &repo_slug) {
            Ok(_lock) => {
                if worktree::idle_count(state, &repo_slug) < idle_pool_max as usize {
                    let probes = worktree::probe(path, &record.base_commit);
                    if matches!(
                        worktree::decide(&probes),
                        worktree::PruneDecision::Remove
                            | worktree::PruneDecision::ArchiveThenRemove(_)
                    ) {
                        let _ = worktree::update_status(
                            state,
                            &repo_slug,
                            path,
                            worktree::WorktreeStatus::Idle,
                            None,
                        );
                        return ReclaimOutcome::Idled;
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "--worktree {}: could not lock the worktree store ({e}); falling back to a \
                     normal removal instead of idling",
                    path.display()
                );
            }
        }
    }
    match worktree::prune_one(state, repo, &repo_slug, path, &record.base_commit) {
        worktree::PruneOutcome::Removed | worktree::PruneOutcome::RemovedWithSkipped(_) => {
            ReclaimOutcome::Removed
        }
        worktree::PruneOutcome::Archived { dest, .. } => ReclaimOutcome::Archived(dest),
        worktree::PruneOutcome::Kept(reason) => ReclaimOutcome::InspectionFailed {
            probe: reason.probe,
            note: reason.note,
        },
        worktree::PruneOutcome::Failed(reason) => ReclaimOutcome::Failed(reason),
    }
}

/// Only reclaim canonical agent-allocated trees under this repo; never take ownership of an explicit workdir.
pub(crate) fn is_agent_managed_worktree(repo: &Path, cwd: &Path) -> bool {
    let Ok(repo) = std::fs::canonicalize(repo) else {
        return false;
    };
    let Ok(cwd) = std::fs::canonicalize(cwd) else {
        return false;
    };
    let root = repo.join(crate::utils::SCRIPT_DIR_NAME).join("worktrees");
    cwd.starts_with(&root)
}

/// Share reclaim reporting between explicit completion and Drop so both paths expose failures consistently.
pub(super) fn reclaim_worktree_and_report(
    state: &StateDir,
    repo: &Path,
    path: &Path,
    idle_pool_max: u32,
) {
    let short = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("?");
    match reclaim_worktree(state, repo, path, idle_pool_max) {
        ReclaimOutcome::Idled => {
            eprintln!(
                "--worktree {}: idled; kept warm for the next `--worktree-reuse` allocation with a \
                 matching base commit",
                path.display()
            );
        }
        ReclaimOutcome::Removed => {
            eprintln!(
                "--worktree {}: reclaimed; branch {short} keeps the worker's commits",
                path.display()
            );
        }
        ReclaimOutcome::Archived(dest) => {
            eprintln!(
                "--worktree {}: untracked/ignored content archived to {}, then reclaimed; branch {short} \
                 keeps the worker's commits",
                path.display(),
                dest.display()
            );
        }
        ReclaimOutcome::InspectionFailed { probe, note } => {
            eprintln!(
                "--worktree {}: left in place ({probe}: {note}); inspect and remove manually, or \
                 `zirv ctx worktree prune {}`",
                path.display(),
                path.display()
            );
        }
        ReclaimOutcome::Failed(reason) => {
            eprintln!("--worktree {}: left in place ({reason})", path.display());
        }
    }
}

/// Arm immediately after allocation so every later error/refusal reclaims the unused tree.
/// Disarm only after explicit reclaim or real pane ownership transfer; a refused dashboard join retains ownership.
pub(super) struct WorktreeReclaimGuard<'a> {
    state: &'a StateDir,
    repo: &'a Path,
    path: Option<PathBuf>,
    idle_pool_max: u32,
}

impl<'a> WorktreeReclaimGuard<'a> {
    pub(super) fn new(
        state: &'a StateDir,
        repo: &'a Path,
        path: Option<PathBuf>,
        idle_pool_max: u32,
    ) -> Self {
        Self {
            state,
            repo,
            path,
            idle_pool_max,
        }
    }

    /// Disarm after ownership transfers so Drop cannot reclaim the same tree twice.
    pub(super) fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for WorktreeReclaimGuard<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            reclaim_worktree_and_report(self.state, self.repo, &path, self.idle_pool_max);
        }
    }
}

/// Pure selection: validated workdir overrides repo for both child cwd and sandbox (#228).
pub(crate) fn effective_launch_repo(workdir: Option<&Path>, repo: &Path) -> PathBuf {
    workdir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo.to_path_buf())
}

/// Bound filesystem candidate probes so large prompts cannot stall dispatch.
const MAX_WORKDIR_WARNING_CANDIDATES: usize = 32;

/// Strip prose punctuation and closing wrappers before path classification (#249, #250).
const TRAILING_TOKEN_PUNCTUATION: [char; 8] = ['.', ',', ';', ':', ')', '"', '\'', '`'];

/// Strip leading backticks/parentheses so wrapped absolute paths remain candidates; quotes are split boundaries (#249, #250).
const LEADING_TOKEN_WRAPPING: [char; 2] = ['`', '('];

/// Pure Unix, drive-letter and UNC path classification, independent of host filesystem existence.
fn looks_like_absolute_path_token(token: &str) -> bool {
    if token.starts_with('/') || token.starts_with("~/") || token.starts_with(r"\\") {
        return true;
    }
    let mut chars = token.chars();
    let Some(drive) = chars.next() else {
        return false;
    };
    drive.is_ascii_alphabetic()
        && chars.next() == Some(':')
        && matches!(chars.next(), Some('\\') | Some('/'))
}

/// Bound candidate parsing before filesystem probes; classify independently of disk existence.
fn candidate_path_tokens(prompt: &str) -> impl Iterator<Item = &str> {
    prompt
        .split(|c: char| c.is_whitespace() || c == '\'' || c == '"')
        .filter(|token| !token.is_empty())
        .map(|token| {
            token
                .trim_end_matches(TRAILING_TOKEN_PUNCTUATION)
                .trim_start_matches(LEADING_TOKEN_WRAPPING)
        })
        .filter(|token| looks_like_absolute_path_token(token))
        .take(MAX_WORKDIR_WARNING_CANDIDATES)
}

/// Warn early about existing paths outside the sandbox to avoid workers spending a run blocked by scope (#250).
/// Only filesystem probes are impure; discard nonexistent/internal paths because false positives are unacceptable.
fn out_of_repo_paths_in_prompt(
    prompt: &str,
    launch_repo: &Path,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let Ok(canonical_repo) = std::fs::canonicalize(launch_repo) else {
        return Vec::new();
    };
    candidate_path_tokens(prompt)
        .filter_map(|token| {
            let candidate: PathBuf = match token.strip_prefix("~/") {
                Some(rest) => home?.join(rest),
                None => PathBuf::from(token),
            };
            let canonical = std::fs::canonicalize(&candidate).ok()?;
            (!canonical.starts_with(&canonical_repo)).then_some(canonical)
        })
        .collect()
}

/// Advise same-harness callers toward visible native subagents; never refuse or hint for work-group/scope dispatches (#328).
pub(super) fn same_harness_hint(args: &AgentArgs, env: EnvLookup<'_>) -> Option<String> {
    if args.group.is_some() || args.scope.is_some() || env(WORK_GROUP_ENV).is_some() {
        return None;
    }
    let running = env(super::super::adapters::AGENT_ENV)?;
    if !running.eq_ignore_ascii_case(args.name.trim()) {
        return None;
    }
    Some(format!(
        "hint: this session already runs under {running}; for same-harness delegation use the \
         harness's native subagent tool (visible here, result returned directly) and keep `zirv \
         agent` for another harness or a work group -- proceeding anyway"
    ))
}

/// Nonfatal path warnings; explicit workdir suppresses them because it already declares an intentional alternate root.
pub(super) fn warn_about_paths_outside_launch_repo(
    prompt: &str,
    launch_repo: &Path,
    home: Option<&Path>,
) {
    for path in out_of_repo_paths_in_prompt(prompt, launch_repo, home) {
        eprintln!(
            "warning: brief references {}, which is outside the worker's writable root {}; if \
             the work targets that location, dispatch with --workdir <its repo root>",
            path.display(),
            launch_repo.display()
        );
    }
}

pub(crate) fn codex_read_only_build_warning(
    adapter_name: &str,
    mode: WorkerMode,
) -> Option<&'static str> {
    (adapter_name.eq_ignore_ascii_case("codex") && mode == WorkerMode::ReadOnly).then_some(
        "codex --sandbox read-only denies every write, including target/ and cargo's registry cache, \
         so build/test commands fail in this seat; for a seat that must compile use the default \
         writing mode with a brief that forbids source edits",
    )
}

/// Codex on Windows denies resolved linked-worktree gitdirs even when writable roots include them (#364).
pub(super) fn codex_worktree_sandbox_warning(
    adapter_name: &str,
    git_dirs: Option<(PathBuf, PathBuf)>,
    windows: bool,
) -> Option<String> {
    if !windows || !adapter_name.eq_ignore_ascii_case("codex") {
        return None;
    }
    let (git_dir, common_dir) = git_dirs?;
    if git_dir == common_dir {
        return None;
    }
    Some(format!(
        "warning: codex's Windows sandbox denies writes to the worktree gitdir {}; the worker \
         can build and test but not stage or commit -- leave commits to the orchestrator",
        git_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;
    use std::path::PathBuf;

    use super::super::tests::*;

    #[test]
    fn refused_workdir_roots_cover_operator_state_and_harness_homes() {
        let home = if cfg!(windows) {
            Path::new(r"C:\Users\operator")
        } else {
            Path::new("/home/operator")
        };
        let homes = WorkdirHomes::new(home, &home.join("state/zirv/ctx"), &|_| None);
        let root = home.ancestors().last().unwrap();
        assert_eq!(refused_workdir_root(root, &homes), Some("filesystem root"));
        assert_eq!(refused_workdir_root(home, &homes), Some("$HOME"));
        for (name, path) in &homes.roots {
            for target in [path.clone(), path.join("projects/session")] {
                assert!(
                    refused_workdir_root(&target, &homes).is_some(),
                    "{name}: {}",
                    target.display()
                );
            }
        }
        for target in [
            home.join("checkout"),
            home.join(".claude-sibling"),
            home.join(".ssh-sibling"),
        ] {
            assert_eq!(
                refused_workdir_root(&target, &homes),
                None,
                "{}",
                target.display()
            );
        }
        for key in [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "COPILOT_HOME",
            "XDG_DATA_HOME",
            "PI_CODING_AGENT_DIR",
            "QWEN_HOME",
            "QWEN_RUNTIME_DIR",
        ] {
            let custom = home.join("custom");
            let homes = WorkdirHomes::new(home, &home.join("state"), &|name| {
                (name == key).then(|| custom.to_string_lossy().into_owned())
            });
            let target = if key == "XDG_DATA_HOME" {
                custom.join("opencode")
            } else {
                custom
            };
            assert!(
                refused_workdir_root(&target.join("sessions"), &homes).is_some(),
                "{key}"
            );
        }
    }

    #[test]
    fn validate_workdir_rejects_a_directory_that_does_not_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let err = validate_workdir(&missing).expect_err("a missing directory must be refused");
        assert!(err.to_string().contains("--workdir"), "got {err}");
    }

    #[test]
    fn validate_workdir_rejects_a_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("plain-file");
        std::fs::write(&file, "x").expect("write");
        let err = validate_workdir(&file).expect_err("a file is not a directory");
        assert!(err.to_string().contains("is not a directory"), "got {err}");
    }

    /// The exact wording issue #228's own acceptance criteria specify:
    /// "error: --workdir <dir> is not inside a git repository; zirv agent
    /// workers need a repository checkout" (the "error: " prefix is added by
    /// `crate::output::error` at the top-level dispatcher, not by this
    /// function -- see its own doc comment).
    #[test]
    fn validate_workdir_rejects_a_directory_with_no_git_ancestry() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let plain = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&plain).expect("mkdir");
        let err = validate_workdir(&plain).expect_err("not a git repo");
        let msg = err.to_string();
        assert!(msg.contains("--workdir"), "got {msg}");
        assert!(
            msg.contains(
                "is not inside a git repository; zirv agent workers need a repository \
                          checkout"
            ),
            "must match issue #228's exact wording: {msg}"
        );
    }

    /// No escape hatch (issue #228, decision 1): unlike, say, codex's own
    /// `--skip-git-repo-check`, a non-repo `--workdir` has no override --
    /// `validate_workdir` takes no such flag at all.
    #[test]
    fn validate_workdir_accepts_a_real_git_repo_and_canonicalises_it() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("a-repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let canon = validate_workdir(&repo).expect("a real repo checkout is fine");
        assert_eq!(canon, std::fs::canonicalize(&repo).expect("canonicalize"));
    }

    /// Issue #267, acceptance criterion: `--worktree` allocates
    /// `<repo>/.zirv/worktrees/<short>` via `git worktree add` from the
    /// session's base -- a real linked worktree, not merely a directory
    /// that happens to sit at that path.
    #[test]
    fn allocate_worktree_creates_a_real_linked_worktree_under_the_repo() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&["config", "user.email", "test@example.com"]));
        assert!(run(&["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&["add", "README.md"]));
        assert!(run(&["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree =
            allocate_worktree(&state, &repo, None, false, &[]).expect("allocate a fresh worktree");

        assert!(
            worktree.starts_with(
                std::fs::canonicalize(&repo)
                    .expect("canonicalize")
                    .join(crate::utils::SCRIPT_DIR_NAME)
                    .join("worktrees")
            ),
            "must live under <repo>/.zirv/worktrees/: {}",
            worktree.display()
        );
        assert!(
            worktree.is_dir(),
            "the worktree must actually exist on disk"
        );
        assert!(
            adapters::git_common_dir(&worktree).is_some(),
            "the allocated path must be a real git working tree"
        );
        assert_eq!(
            adapters::git_common_dir(&worktree),
            adapters::git_common_dir(&repo),
            "the linked worktree must share the session base's own .git"
        );
    }

    /// A second `--worktree` allocation from the same session base must
    /// never collide with the first -- each gets its own fresh short id.
    #[test]
    fn allocate_worktree_never_collides_across_two_calls() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&["config", "user.email", "test@example.com"]));
        assert!(run(&["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&["add", "README.md"]));
        assert!(run(&["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let first = allocate_worktree(&state, &repo, None, false, &[]).expect("first allocation");
        let second = allocate_worktree(&state, &repo, None, false, &[]).expect("second allocation");
        assert_ne!(first, second, "each --worktree call must get its own tree");
    }

    /// Issue #718 acceptance criterion: a matching `Idle` record, reused via
    /// `--worktree --worktree-reuse`, is reset in place -- no new directory
    /// is created, and its warm build directory (a stand-in `target/`) is
    /// still on disk afterward.
    #[test]
    fn allocate_worktree_reuse_matches_the_ordered_setup_list_and_rejects_a_changed_list() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let setup = vec!["cargo fetch".to_string(), "cargo build".to_string()];
        let first = allocate_worktree(&state, &repo, None, true, &setup).expect("first allocation");
        // A stand-in warm build directory the pool is supposed to preserve.
        std::fs::create_dir_all(first.join("target")).expect("mkdir target");
        std::fs::write(first.join("target/warm"), "cached\n").expect("write warm cache");

        assert_eq!(
            reclaim_worktree(&state, &repo, &first, 4),
            ReclaimOutcome::Idled,
            "a reuse-eligible, clean tree must be idled, not removed"
        );
        assert!(first.is_dir(), "an idled tree must stay on disk");

        let second =
            allocate_worktree(&state, &repo, None, true, &setup).expect("reuse allocation");
        assert_eq!(
            second, first,
            "a matching idle tree must be reused in place"
        );
        assert_eq!(
            std::fs::read_to_string(second.join("target/warm")).expect("read warm cache"),
            "cached\n",
            "the warm build directory must survive reuse"
        );

        assert_eq!(
            reclaim_worktree(&state, &repo, &second, 4),
            ReclaimOutcome::Idled
        );
        let changed_setup = vec!["cargo fetch".to_string(), "cargo test".to_string()];
        let third = allocate_worktree(&state, &repo, None, true, &changed_setup)
            .expect("changed setup falls back to a cold allocation");
        assert_ne!(
            third, second,
            "a changed ordered setup list must not reuse a tree prepared for different commands"
        );
    }

    /// A digest mismatch (a different base commit since the tree was idled)
    /// must never force reuse -- the delegation falls back to a fresh cold
    /// worktree instead.
    #[test]
    fn allocate_worktree_reuse_falls_back_to_cold_on_a_different_base_commit() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let first = allocate_worktree(&state, &repo, None, true, &[]).expect("first allocation");
        assert_eq!(
            reclaim_worktree(&state, &repo, &first, 4),
            ReclaimOutcome::Idled
        );

        // The repo's HEAD moves on -- a fresh commit changes the base commit
        // any later `--worktree-reuse` allocation would digest against.
        std::fs::write(repo.join("README.md"), "hello again\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "second"]));

        let second = allocate_worktree(&state, &repo, None, true, &[]).expect("cold fallback");
        assert_ne!(
            second, first,
            "a digest mismatch must never force reuse of the stale tree"
        );
    }

    /// The mandated safety test: an `Idle` tree with uncommitted (tracked)
    /// changes is NEVER reused and NEVER reset -- the delegation falls back
    /// to a cold worktree, and the dirty tree is left intact, byte-for-byte.
    #[test]
    fn allocate_worktree_reuse_never_reuses_or_resets_a_dirty_idle_tree() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let first = allocate_worktree(&state, &repo, None, true, &[]).expect("first allocation");
        assert_eq!(
            reclaim_worktree(&state, &repo, &first, 4),
            ReclaimOutcome::Idled
        );
        // Dirty the idled tree with an uncommitted TRACKED change -- exactly
        // the condition `worktree::decide` refuses a plain prune over too.
        std::fs::write(first.join("README.md"), "dirtied while idle\n").expect("write");

        let second = allocate_worktree(&state, &repo, None, true, &[]).expect("cold fallback");
        assert_ne!(
            second, first,
            "a dirty idle tree must never be handed back out"
        );
        assert!(first.is_dir(), "the dirty idle tree must be left in place");
        assert_eq!(
            std::fs::read_to_string(first.join("README.md")).expect("read"),
            "dirtied while idle\n",
            "the dirty content must be untouched -- no `git reset --hard` ever ran against it"
        );
    }

    /// Review finding (2026-09, CRITICAL, issue #718): two concurrent
    /// `--worktree --worktree-reuse` allocations racing for the same single
    /// `Idle`, matching-digest record must never both claim it -- the
    /// per-repo worktree-store lock (`worktree::lock_worktrees`) serializes
    /// select+claim, so the loser always sees the record already claimed
    /// (`Active`) and falls back to a fresh cold worktree instead.
    #[test]
    fn allocate_worktree_reuse_never_lets_two_concurrent_calls_claim_the_same_idle_record() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = std::sync::Arc::new(StateDir::from_root(tmp.path().join("state")));
        let repo = std::sync::Arc::new(repo);
        let first = allocate_worktree(&state, &repo, None, true, &[]).expect("first allocation");
        assert_eq!(
            reclaim_worktree(&state, &repo, &first, 4),
            ReclaimOutcome::Idled
        );

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                let repo = repo.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    allocate_worktree(&state, &repo, None, true, &[]).expect("allocation")
                })
            })
            .collect();
        let results: Vec<PathBuf> = handles
            .into_iter()
            .map(|h| h.join().expect("thread joins"))
            .collect();

        assert_ne!(
            results[0], results[1],
            "two concurrent reuse allocations must never both claim the same idle tree"
        );
        assert!(
            results.contains(&first),
            "exactly one of the two concurrent calls must have reused the idle tree"
        );
    }

    /// Review finding (2026-09, CRITICAL, issue #718): a `git reset --hard`
    /// that fails AFTER the claim record has already been appended must
    /// never leave that record `Idle` for a later call to find again --
    /// `claim_idle_worktree` marks it `InspectionFailed` and returns `None`
    /// so the caller falls back to a fresh cold worktree. The digest is
    /// matched against a real, valid `Idle` record (so `find_reusable`'s own
    /// proof passes); the `base_commit` passed to `claim_idle_worktree`
    /// itself is bogus, standing in for a reset that fails for any reason
    /// after a successful claim.
    #[test]
    fn claim_idle_worktree_falls_back_and_leaves_no_idle_record_when_reset_fails() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let first = allocate_worktree(&state, &repo, None, true, &[]).expect("first allocation");
        assert_eq!(
            reclaim_worktree(&state, &repo, &first, 4),
            ReclaimOutcome::Idled
        );

        let repo_slug = super::super::super::state::repo_slug(&repo);
        let digest = worktree::latest_for_path(&state, &repo_slug, &first)
            .expect("idle record")
            .setup_digest
            .expect("idle record carries a digest");

        let claimed = claim_idle_worktree(&state, &repo_slug, &digest, "not-a-real-commit", None);
        assert!(
            claimed.is_none(),
            "a reset against a bogus commit must fail and never be returned as reused"
        );
        let record = worktree::latest_for_path(&state, &repo_slug, &first)
            .expect("the claimed record must still exist");
        assert_ne!(
            record.status,
            worktree::WorktreeStatus::Idle,
            "a failed reset must never leave the claimed record Idle"
        );
        assert!(
            worktree::find_reusable(&state, &repo_slug, &digest).is_none(),
            "no Idle record must remain for this path after a failed reset"
        );
    }

    /// Issue #718: `reclaim_worktree` never idles a tree its own record did
    /// not opt into pooling (`setup_digest: None`, today's exact default) --
    /// unchanged, byte-identical behavior for every plain `--worktree` call.
    #[test]
    fn reclaim_worktree_never_idles_a_tree_that_never_opted_into_reuse() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree =
            allocate_worktree(&state, &repo, None, false, &[]).expect("plain allocation");
        assert_eq!(
            reclaim_worktree(&state, &repo, &worktree, 4),
            ReclaimOutcome::Removed
        );
        assert!(!worktree.exists());
    }

    /// A full idle pool falls back to a normal proof-required removal
    /// instead of growing past `idle_pool_max`.
    #[test]
    fn reclaim_worktree_falls_back_to_removal_when_the_idle_pool_is_full() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let first = allocate_worktree(&state, &repo, None, true, &[]).expect("first allocation");
        // `idle_pool_max: 0` -- there is never room for even one idle entry.
        assert_eq!(
            reclaim_worktree(&state, &repo, &first, 0),
            ReclaimOutcome::Removed,
            "a zero-capacity pool must fall back to a normal removal"
        );
        assert!(!first.exists());
    }

    /// Review finding (2026-09), acceptance: a genuinely clean allocated
    /// worktree (no commits, no changes at all beyond what `allocate_
    /// worktree` itself minted) is removed by `reclaim_worktree`, and the
    /// branch `git worktree add` minted for it survives.
    #[test]
    fn reclaim_worktree_removes_a_clean_tree_and_keeps_its_branch() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree =
            allocate_worktree(&state, &repo, None, false, &[]).expect("allocate a fresh worktree");
        let short = worktree
            .file_name()
            .and_then(|n| n.to_str())
            .expect("short id")
            .to_string();

        let outcome = reclaim_worktree(&state, &repo, &worktree, 4);
        assert_eq!(outcome, ReclaimOutcome::Removed);
        assert!(
            !worktree.exists(),
            "the worktree directory must be gone after reclamation"
        );

        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("branch")
            .arg("--list")
            .arg(&short)
            .output()
            .expect("git branch --list");
        assert!(
            !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the branch {short} must survive reclamation"
        );
    }

    /// Issue #319: a worktree with a genuine, unpushed commit of its own
    /// (proof the worker actually did something) is now left ENTIRELY in
    /// place by `reclaim_worktree` -- directory and all -- never silently
    /// removed just because `git status --porcelain` happens to read clean.
    /// This tightens the pre-#319 contract (which removed on porcelain-clean
    /// alone, discarding the *directory* while trusting the branch to carry
    /// the commit): now an operator gets a chance to look before even the
    /// directory goes away.
    #[test]
    fn reclaim_worktree_keeps_a_tree_with_an_unpushed_worker_commit() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree =
            allocate_worktree(&state, &repo, None, false, &[]).expect("allocate a fresh worktree");
        std::fs::write(worktree.join("worker-output.txt"), "done\n").expect("write");
        assert!(run(&worktree, &["add", "worker-output.txt"]));
        assert!(run(&worktree, &["commit", "-q", "-m", "worker commit"]));

        let outcome = reclaim_worktree(&state, &repo, &worktree, 4);
        match outcome {
            ReclaimOutcome::InspectionFailed { probe, .. } => assert_eq!(probe, "ahead"),
            other => panic!("expected InspectionFailed(ahead), got {other:?}"),
        }
        assert!(
            worktree.exists(),
            "a tree with an unpushed commit must never be removed automatically"
        );
    }

    /// Review finding (2026-09), extended for issue #319: an allocated
    /// worktree with only untracked content is archived, then removed --
    /// never left in place, and never force-removed without a copy first.
    #[test]
    fn reclaim_worktree_archives_untracked_content_then_removes_the_tree() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("session-base");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let run = |dir: &Path, args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&repo, &["config", "user.email", "test@example.com"]));
        assert!(run(&repo, &["config", "user.name", "test"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(run(&repo, &["add", "README.md"]));
        assert!(run(&repo, &["commit", "-q", "-m", "initial"]));

        let state = StateDir::from_root(tmp.path().join("state"));
        let worktree =
            allocate_worktree(&state, &repo, None, false, &[]).expect("allocate a fresh worktree");
        // An untracked file is enough to make `git status --porcelain`
        // non-empty -- no commit needed.
        std::fs::write(worktree.join("scratch.txt"), "not committed\n").expect("write");

        let outcome = reclaim_worktree(&state, &repo, &worktree, 4);
        match outcome {
            ReclaimOutcome::Archived(dest) => {
                assert_eq!(
                    std::fs::read_to_string(dest.join("scratch.txt")).expect("read archived"),
                    "not committed\n"
                );
            }
            other => panic!("expected Archived, got {other:?}"),
        }
        assert!(
            !worktree.exists(),
            "the worktree must be removed once its untracked content is archived"
        );
    }

    #[test]
    fn effective_launch_repo_prefers_workdir_when_given_and_falls_back_to_repo_otherwise() {
        let repo = Path::new("/current/repo");
        let workdir = Path::new("/other/repo");
        assert_eq!(
            effective_launch_repo(Some(workdir), repo),
            workdir.to_path_buf(),
            "an explicit --workdir must win"
        );
        assert_eq!(
            effective_launch_repo(None, repo),
            repo.to_path_buf(),
            "no --workdir is today's unchanged behaviour"
        );
    }

    #[test]
    fn codex_read_only_build_warning_only_warns_for_codex_read_only() {
        for name in ["codex", "CODEX"] {
            assert_eq!(
                codex_read_only_build_warning(name, WorkerMode::ReadOnly),
                Some(
                    "codex --sandbox read-only denies every write, including target/ and cargo's registry cache, \
                     so build/test commands fail in this seat; for a seat that must compile use the default \
                     writing mode with a brief that forbids source edits"
                )
            );
            assert_eq!(
                codex_read_only_build_warning(name, WorkerMode::Writing),
                None
            );
        }
        for mode in [WorkerMode::ReadOnly, WorkerMode::Writing] {
            assert_eq!(codex_read_only_build_warning("claude", mode), None);
        }
    }

    #[test]
    fn codex_worktree_sandbox_warning_only_warns_for_codex_on_windows_in_a_linked_worktree() {
        let repo = tempfile::tempdir().expect("tempdir");
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "test"]);
        run_git(&["commit", "--allow-empty", "-q", "-m", "init"]);
        let linked = tempfile::tempdir().expect("tempdir");
        let linked_path = linked.path().join("worktree");
        run_git(&["worktree", "add", linked_path.to_str().expect("utf8 path")]);
        let dirs = adapters::git_dirs(&linked_path).expect("linked git dirs");
        assert_eq!(
            codex_worktree_sandbox_warning("codex", Some(dirs.clone()), true),
            Some(format!(
                "warning: codex's Windows sandbox denies writes to the worktree gitdir {}; the worker \
                 can build and test but not stage or commit -- leave commits to the orchestrator",
                dirs.0.display()
            ))
        );
        assert_eq!(
            codex_worktree_sandbox_warning("codex", Some(dirs.clone()), false),
            None
        );
        assert_eq!(
            codex_worktree_sandbox_warning("claude", Some(dirs), true),
            None
        );
        assert_eq!(codex_worktree_sandbox_warning("codex", None, true), None);
    }

    #[test]
    fn codex_worktree_sandbox_warning_is_silent_for_a_main_checkout() {
        let repo = tempfile::tempdir().expect("tempdir");
        assert!(git_init(repo.path()), "git init");
        let dirs = adapters::git_dirs(repo.path()).expect("main git dirs");
        assert_eq!(
            codex_worktree_sandbox_warning("codex", Some(dirs), true),
            None
        );
    }

    /// Issue #250: an existing path outside the launch repo, named in the
    /// prompt, must be flagged so `run_with` can warn toward `--workdir`.
    #[test]
    fn out_of_repo_paths_in_prompt_warns_on_an_absolute_path_outside_the_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(&outside).expect("mkdir outside");

        let prompt = format!("fix the bug described in {}", outside.display());
        let found = out_of_repo_paths_in_prompt(&prompt, &repo, None);

        assert_eq!(
            found,
            vec![std::fs::canonicalize(&outside).expect("canonicalize")],
            "an existing path outside the repo must be flagged"
        );
    }

    /// A path inside the repo is exactly what a worker's writable root
    /// already covers -- nothing to warn about.
    #[test]
    fn out_of_repo_paths_in_prompt_is_silent_for_a_path_inside_the_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let inside = repo.join("src");
        std::fs::create_dir_all(&inside).expect("mkdir");

        let prompt = format!("edit {}", inside.display());
        let found = out_of_repo_paths_in_prompt(&prompt, &repo, None);

        assert!(
            found.is_empty(),
            "a path inside the repo must not be flagged: {found:?}"
        );
    }

    /// A path that does not exist on disk cannot be a real cross-repo
    /// target -- likely a flag value, an example, or plain prose -- so it is
    /// silently dropped rather than risking a false-positive warning.
    #[test]
    fn out_of_repo_paths_in_prompt_is_silent_for_a_path_that_does_not_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let missing = tmp.path().join("nonexistent");

        let prompt = format!("look at {}", missing.display());
        let found = out_of_repo_paths_in_prompt(&prompt, &repo, None);

        assert!(
            found.is_empty(),
            "a nonexistent path must not be flagged: {found:?}"
        );
    }

    /// `~/` tokens expand against the given home dir before the exists/
    /// inside-repo checks, the same as a shell would expand them.
    #[test]
    fn out_of_repo_paths_in_prompt_expands_a_tilde_path_via_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        let outside = home.join("elsewhere");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(&outside).expect("mkdir outside");

        let prompt = "please work on ~/elsewhere today";
        let found = out_of_repo_paths_in_prompt(prompt, &repo, Some(&home));

        assert_eq!(
            found,
            vec![std::fs::canonicalize(&outside).expect("canonicalize")],
            "a ~/ path must expand against the given home dir: {found:?}"
        );
    }

    /// A `~/` token is simply not a candidate at all when no home dir is
    /// known -- it must not be misread as a literal `~` directory.
    #[test]
    fn out_of_repo_paths_in_prompt_skips_a_tilde_path_with_no_home_given() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let prompt = "please work on ~/elsewhere today";
        let found = out_of_repo_paths_in_prompt(prompt, &repo, None);

        assert!(
            found.is_empty(),
            "no home dir means no expansion: {found:?}"
        );
    }

    /// The scan is capped at `MAX_WORKDIR_WARNING_CANDIDATES` path-like
    /// tokens so a huge prompt cannot make dispatch slow -- exercised with
    /// more real, existing, out-of-repo directories than the cap allows.
    #[test]
    fn out_of_repo_paths_in_prompt_caps_the_number_of_candidates_scanned() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut prompt = String::new();
        let mut dirs = Vec::new();
        for i in 0..(MAX_WORKDIR_WARNING_CANDIDATES + 8) {
            let dir = tmp.path().join(format!("outside-{i}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            prompt.push_str(&dir.display().to_string());
            prompt.push(' ');
            dirs.push(dir);
        }

        let found = out_of_repo_paths_in_prompt(&prompt, &repo, None);

        assert_eq!(
            found.len(),
            MAX_WORKDIR_WARNING_CANDIDATES,
            "must cap the scan at {MAX_WORKDIR_WARNING_CANDIDATES} candidates: {found:?}"
        );
        let last = std::fs::canonicalize(dirs.last().unwrap()).expect("canonicalize");
        assert!(
            !found.contains(&last),
            "a candidate beyond the cap must never be checked: {found:?}"
        );
    }

    /// Fix 6 (issue #249/#250 review): a path wrapped in backticks -- a
    /// common way to set a path apart in prose -- must still be recognized
    /// and flagged when it exists and resolves outside the repo. Before
    /// this fix the leading backtick was never stripped, so `starts_with`
    /// failed and the whole token was silently dropped as a candidate.
    #[test]
    fn out_of_repo_paths_in_prompt_warns_on_a_backtick_wrapped_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(&outside).expect("mkdir outside");

        let prompt = format!("fix the bug described in `{}`", outside.display());
        let found = out_of_repo_paths_in_prompt(&prompt, &repo, None);

        assert_eq!(
            found,
            vec![std::fs::canonicalize(&outside).expect("canonicalize")],
            "a backtick-wrapped existing path outside the repo must still be flagged: {found:?}"
        );
    }

    /// The parenthesized shape from the same fix: `(/path)` must resolve to
    /// the same candidate a bare `/path` would.
    #[test]
    fn out_of_repo_paths_in_prompt_warns_on_a_parenthesized_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(&outside).expect("mkdir outside");

        let prompt = format!("see ({}) for context", outside.display());
        let found = out_of_repo_paths_in_prompt(&prompt, &repo, None);

        assert_eq!(
            found,
            vec![std::fs::canonicalize(&outside).expect("canonicalize")],
            "a parenthesized existing path outside the repo must still be flagged: {found:?}"
        );
    }

    /// Fix 6: the tokenizer/classifier alone (no filesystem, so Windows
    /// shapes are covered on any host, not just a Windows CI runner) must
    /// recognize a backtick- or paren-wrapped Unix path as a candidate.
    #[test]
    fn candidate_path_tokens_strips_wrapping_punctuation_from_both_ends() {
        let found: Vec<&str> =
            candidate_path_tokens("see `/tmp/one` and (/tmp/two) and plain/tmp/three").collect();
        assert_eq!(
            found,
            vec!["/tmp/one", "/tmp/two"],
            "wrapping backticks and parens must be stripped from both ends: {found:?}"
        );
    }

    /// Fix 6: a Windows drive-letter absolute path (`C:\...` or `C:/...`)
    /// must be recognized as a candidate token, purely by shape -- tested
    /// separately from `out_of_repo_paths_in_prompt`'s own `exists()` gate
    /// (a drive-relative path cannot exist on a non-Windows host) so this
    /// coverage does not depend on the host platform.
    #[test]
    fn looks_like_absolute_path_token_recognizes_windows_drive_letter_paths() {
        assert!(looks_like_absolute_path_token(r"C:\Users\jane\project"));
        assert!(looks_like_absolute_path_token("C:/Users/jane/project"));
        assert!(looks_like_absolute_path_token(r"d:\data"));
        assert!(
            !looks_like_absolute_path_token("C:notanabsolutepath"),
            "a bare drive-relative token with no separator is not absolute"
        );
        assert!(
            !looks_like_absolute_path_token("relative/path"),
            "an ordinary relative path is still not a candidate"
        );
    }

    /// Fix 6: a Windows UNC path (`\\server\share\...`) must also be
    /// recognized as a candidate token.
    #[test]
    fn looks_like_absolute_path_token_recognizes_windows_unc_paths() {
        assert!(looks_like_absolute_path_token(
            r"\\server\share\project\file.txt"
        ));
        assert!(
            !looks_like_absolute_path_token(r"\single\backslash"),
            "a single leading backslash is not a UNC path"
        );
    }

    /// Fix 6: a Windows-shaped token wrapped in backticks or parens must
    /// also survive the wrapping-punctuation strip, the same as the Unix
    /// shapes above.
    #[test]
    fn candidate_path_tokens_recognizes_a_wrapped_windows_drive_letter_path() {
        let found: Vec<&str> =
            candidate_path_tokens(r"see `C:\Users\jane\project` please").collect();
        assert_eq!(found, vec![r"C:\Users\jane\project"], "got {found:?}");

        let found: Vec<&str> =
            candidate_path_tokens(r"see (\\server\share\project) please").collect();
        assert_eq!(found, vec![r"\\server\share\project"], "got {found:?}");
    }

    /// Review finding (2026-09), finding 2a: `is_agent_managed_worktree`
    /// recognises exactly the paths `allocate_worktree` itself creates, and
    /// nothing else -- the dashboard's own pane-reap path relies on this to
    /// decide whether it may reclaim a just-exited pane's cwd.
    #[test]
    fn is_agent_managed_worktree_recognizes_only_paths_under_zirv_worktrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let managed = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join("worktrees")
            .join("abcd1234");
        let unmanaged = repo.join("some-other-dir");
        std::fs::create_dir_all(&managed).expect("mkdir managed");
        std::fs::create_dir_all(&unmanaged).expect("mkdir unmanaged");

        assert!(
            is_agent_managed_worktree(&repo, &managed),
            "a path under <repo>/.zirv/worktrees/ must be recognised"
        );
        assert!(
            !is_agent_managed_worktree(&repo, &unmanaged),
            "an arbitrary directory under repo must never be recognised"
        );
        assert!(
            !is_agent_managed_worktree(&repo, &repo),
            "the repo itself is not one of its own worktrees"
        );
        assert!(
            !is_agent_managed_worktree(&repo, &tmp.path().join("does-not-exist")),
            "a path that cannot be canonicalized must never match"
        );
    }

    // -- same_harness_hint (issue #328) --------------------------------------

    #[test]
    fn same_harness_hint_fires_only_for_the_running_harness_outside_a_work_group() {
        let env = env_map(&[(super::super::super::adapters::AGENT_ENV, "claude")]);
        let lookup = |k: &str| env.get(k).cloned();
        let hint = same_harness_hint(&args_for("claude", "brief"), &lookup)
            .expect("claude from a claude seat hints");
        assert!(hint.contains("native subagent tool"), "got: {hint}");
        assert!(hint.contains("proceeding anyway"), "got: {hint}");
        assert!(
            same_harness_hint(&args_for("codex", "brief"), &lookup).is_none(),
            "another harness is exactly what zirv agent is for"
        );
        assert!(
            same_harness_hint(&args_for("Claude", "brief"), &lookup).is_some(),
            "adapter names match case-insensitively like dispatch"
        );

        let grouped = AgentArgs {
            group: Some("wg-1".to_string()),
            ..args_for("claude", "brief")
        };
        assert!(same_harness_hint(&grouped, &lookup).is_none());
        let scoped = AgentArgs {
            role: Some("sub-orchestrator".to_string()),
            scope: Some("area".to_string()),
            ..args_for("claude", "brief")
        };
        assert!(same_harness_hint(&scoped, &lookup).is_none());

        let inherited = env_map(&[
            (super::super::super::adapters::AGENT_ENV, "claude"),
            (WORK_GROUP_ENV, "wg-2"),
        ]);
        assert!(
            same_harness_hint(&args_for("claude", "brief"), &|k| inherited.get(k).cloned())
                .is_none(),
            "an inherited work group is a zirv dispatch by design"
        );
        let unset = env_map(&[]);
        assert!(
            same_harness_hint(&args_for("claude", "brief"), &|k| unset.get(k).cloned()).is_none(),
            "no running-harness evidence, no hint"
        );
    }
}
