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

/// The canonical repository identity used by [`super::state::
/// workflow_identity_slug`]: for a linked `git worktree add` checkout (its
/// own gitdir differs from the shared common dir) this is the common dir's
/// own parent -- the main checkout's working-tree root, since every
/// worktree's `--git-common-dir` resolves back to that main checkout's
/// `.git` directory -- and `path` itself, canonicalized, for everything else
/// (a main checkout, a bare repository, or anywhere `git` does not resolve
/// at all: no `git` on `PATH`, or not a repository).
///
/// Deliberately NOT wired into `state::repo_slug` itself (issue #467
/// review): most `repo_slug` consumers must stay keyed by the literal
/// checkout a process is actually in, never merged across worktrees --
/// `workflow_identity_slug`'s own doc comment lists the two call sites this
/// is reserved for and why.
///
/// A relocated main-checkout `.git` (`git init --separate-git-dir=...`)
/// breaks the "common dir's parent is the main checkout" assumption -- a
/// known, accepted limitation (see the accompanying design note), not
/// attempted here.
///
/// Memoized per canonical path with no invalidation for the life of the
/// process: resolving this shells out to `git`, and `workflow_identity_slug`
/// is called from a hook that fires once per agent turn, so paying that cost
/// more than once per path per process would be wasteful. Safe only because
/// both of `workflow_identity_slug`'s consumers ask a question ("is this the
/// same repository as that one") that cannot change out from under a single
/// process, and neither runs as a long-lived daemon that would accumulate
/// entries for many unrelated repositories over time -- a future caller with
/// either property must not reuse this cache uncritically.
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

/// Every checkout of `path`'s repository -- the main checkout plus every
/// linked `git worktree add` sibling of it, each as its own canonicalized
/// path -- via `git worktree list --porcelain`, run with the same
/// environment isolation `adapters::git_dirs` applies (an inherited
/// `GIT_DIR`/`GIT_COMMON_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` would make
/// this resolve the wrong repository entirely). Empty when `path` is not
/// inside a git working tree or `git` cannot be resolved; the returned list
/// always includes `path`'s own checkout when it succeeds, so callers that
/// want "every OTHER checkout" filter it out themselves.
///
/// Reserved for `workflow::verification::latest_is_fresh_and_passing`'s
/// widened read (issue #467 review): report storage itself stays keyed by
/// the literal checkout (`state::repo_slug`, not `workflow_identity_slug`),
/// so two sibling worktrees never clobber each other's `zirv test changed`
/// evidence -- this is what lets the gate still find a sibling's own fresh,
/// passing evidence for that sibling's own tree. Not memoized: called far
/// less often than `worktree_identity` (once per gate check, not once per
/// agent turn), and its answer is a list of paths a caller then reads
/// evidence for one at a time regardless.
pub(crate) fn sibling_checkouts(path: &Path) -> Vec<PathBuf> {
    let output = std::process::Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(path)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|raw| std::fs::canonicalize(raw).ok())
        .collect()
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
    /// since that identity is what `state::workflow_identity_slug` keys
    /// workflow-state lookup and the Test/Verify evidence gate's read side
    /// under -- never `repo_slug` itself, which every other consumer keeps
    /// keyed by the literal checkout.
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

    /// `sibling_checkouts` must list both the main checkout and its linked
    /// worktree -- this is what lets the Test/Verify gate widen its read to
    /// a sibling's own evidence without merging where evidence is written.
    #[test]
    fn sibling_checkouts_lists_the_main_checkout_and_its_linked_worktree() {
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

        let from_main = sibling_checkouts(main.path());
        let from_worktree = sibling_checkouts(worktree.path());
        let main_canonical = main.path().canonicalize().unwrap();
        let worktree_canonical = worktree.path().canonicalize().unwrap();
        assert!(from_main.contains(&main_canonical), "{from_main:?}");
        assert!(from_main.contains(&worktree_canonical), "{from_main:?}");
        assert_eq!(from_main, from_worktree);
    }

    /// A non-git path (or one where `git` cannot resolve) must yield an
    /// empty list, never a single-entry list containing itself -- callers
    /// combine this with the path they already have.
    #[test]
    fn sibling_checkouts_is_empty_for_a_non_git_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sibling_checkouts(dir.path()), Vec::<PathBuf>::new());
    }
}
