use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Canonicalizes the longest existing prefix, then restores any missing tail.
/// A dangling symlink or inaccessible prefix is unresolvable, not a missing tail.
pub(crate) fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    }
    let tail = path.strip_prefix(existing).ok()?;
    std::fs::canonicalize(existing)
        .ok()
        .map(|root| root.join(tail))
}

/// The canonical repository identity for per-repository state keys (see
/// `state::repo_slug`): for a linked `git worktree add` checkout (its own
/// gitdir differs from the shared common dir) this is the common dir's own
/// parent -- the main checkout's working-tree root, since every worktree's
/// `--git-common-dir` resolves back to that main checkout's `.git` directory
/// -- and `path` itself, canonicalized, for everything else (a main checkout,
/// a bare repository, or anywhere `git` does not resolve at all: no `git` on
/// `PATH`, or not a repository). This is what lets a workflow started in a
/// main checkout be found, and gated, from any of its linked worktree
/// siblings and vice versa (issue #467): every worktree of one repository
/// shares its `.git` common dir, so this resolves them all to one identity.
///
/// A relocated main-checkout `.git` (`git init --separate-git-dir=...`)
/// breaks the "common dir's parent is the main checkout" assumption -- a
/// known, accepted limitation (see the accompanying design note), not
/// attempted here.
///
/// Memoized per canonical path: `repo_slug` is called from hot paths (a hook
/// fires on every tool call), and resolving this shells out to `git`, so
/// paying that cost more than once per path per process would be a real
/// regression.
pub(crate) fn worktree_identity(path: &Path) -> PathBuf {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut cache = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(identity) = cache.get(&canonical) {
        return identity.clone();
    }
    let identity = match super::adapters::git_dirs(&canonical) {
        Some((git_dir, common_dir)) if git_dir != common_dir => common_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| canonical.clone()),
        _ => canonical.clone(),
    };
    cache.insert(canonical.clone(), identity.clone());
    identity
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.example")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.example")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// The core of issue #467: a linked worktree and its main checkout are
    /// two different literal paths that must still resolve to one identity,
    /// since that identity is what `repo_slug` keys every piece of
    /// per-repository state (workflow state, verification reports,
    /// telemetry) under.
    #[test]
    fn a_linked_worktree_resolves_to_its_main_checkouts_identity() {
        let main = tempfile::tempdir().unwrap();
        assert!(git(main.path(), &["init", "-q"]));
        std::fs::write(main.path().join("f.txt"), "base\n").unwrap();
        assert!(git(main.path(), &["add", "."]));
        assert!(git(main.path(), &["commit", "-q", "-m", "base"]));

        let worktree = tempfile::tempdir().unwrap();
        std::fs::remove_dir(worktree.path()).unwrap();
        assert!(git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.path().to_str().unwrap(),
            ]
        ));

        let main_identity = worktree_identity(main.path());
        let linked_identity = worktree_identity(worktree.path());
        assert_eq!(main_identity, linked_identity);
        assert_eq!(main_identity, main.path().canonicalize().unwrap());
    }

    /// The ordinary single-checkout case (no linked worktree at all) must
    /// resolve to itself, unchanged -- this is what keeps every existing
    /// repository's state at its pre-#467 location.
    #[test]
    fn a_plain_checkout_resolves_to_itself() {
        let repo = tempfile::tempdir().unwrap();
        assert!(git(repo.path(), &["init", "-q"]));
        assert_eq!(
            worktree_identity(repo.path()),
            repo.path().canonicalize().unwrap()
        );
    }

    /// A path that is not a git repository at all (or where `git` cannot be
    /// resolved) must fall back to its own canonical form, exactly the
    /// pre-#467 `repo_slug` behavior -- never treated as a linked worktree.
    #[test]
    fn a_non_git_path_falls_back_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            worktree_identity(dir.path()),
            dir.path().canonicalize().unwrap()
        );
    }
}
