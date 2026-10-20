//! What a before-done consult reads beyond `git diff --stat HEAD` of the session cwd (#921):
//! commits made since the session started in this repo's linked worktrees, and the agent's own
//! final message with the commits it cites, resolved against git. All of it is untrusted data
//! the consult weighs, never instructions. Git and transcript I/O lives here, not in `rulings`.

use std::path::{Path, PathBuf};

use super::{StateDir, git_stdout};

pub(super) const WORKTREES_LABEL: &str = "Linked worktrees: ";
pub(super) const CLAIM_LABEL: &str = "Agent's final message (untrusted claim): ";
pub(super) const CITED_LABEL: &str = "Cited by the agent: ";

const WORKTREES_MAX: usize = 6;
const COMMITS_PER_WORKTREE: usize = 3;
const CLAIM_BYTES: usize = 800;
const CITED_MAX: usize = 5;
const SHA_CANDIDATES_MAX: usize = 20;
/// Without a session record the start time is unknown: look back one day.
const FALLBACK_WINDOW_SECS: u64 = 24 * 60 * 60;

pub(super) struct DoneEvidence {
    /// Evidence lines for the consult prompt; empty when there is nothing beyond the cwd diff.
    pub lines: String,
    /// Stable while the commits and cited refs are unchanged. The agent's prose is excluded, so
    /// restating the same claim never reads as new evidence.
    pub fingerprint: String,
}

/// `(path, branch)` per entry of `git worktree list --porcelain`; a detached HEAD has no branch.
fn parse_worktrees(porcelain: &str) -> Vec<(PathBuf, String)> {
    porcelain
        .split("\n\n")
        .filter_map(|block| {
            let path = block
                .lines()
                .find_map(|line| line.strip_prefix("worktree "))?;
            let branch = block
                .lines()
                .find_map(|line| line.strip_prefix("branch refs/heads/"))
                .unwrap_or("(detached)");
            Some((PathBuf::from(path), branch.to_string()))
        })
        .collect()
}

/// Commit hashes and subjects on `path`'s HEAD committed at or after `since`.
fn commits_since(path: &Path, since: u64) -> Vec<String> {
    git_stdout(path, &["log", "-n", "20", "--format=%ct\t%h\t%s", "HEAD"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let committed: u64 = fields.next()?.parse().ok()?;
            let (hash, subject) = (fields.next()?, fields.next()?);
            (committed >= since).then(|| format!("{hash} {subject}"))
        })
        .take(COMMITS_PER_WORKTREE)
        .collect()
}

fn worktree_lines(repo: &Path, since: u64) -> Vec<String> {
    let listing = git_stdout(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    // The cwd's own checkout is already what the diff stat shows; only the others are new.
    let own = git_stdout(repo, &["rev-parse", "--show-toplevel"])
        .and_then(|top| std::fs::canonicalize(top).ok());
    parse_worktrees(&listing)
        .into_iter()
        .filter(|(path, _)| std::fs::canonicalize(path).ok() != own)
        .take(WORKTREES_MAX)
        .filter_map(|(path, branch)| {
            let commits = commits_since(&path, since);
            (!commits.is_empty())
                .then(|| format!("- {} [{branch}]: {}", path.display(), commits.join("; ")))
        })
        .collect()
}

/// Lowercase hex runs long enough to be an abbreviated commit id.
fn sha_candidates(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for token in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let is_hex = (7..=40).contains(&token.len())
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if is_hex && !found.contains(&token) {
            found.push(token);
        }
    }
    found.truncate(SHA_CANDIDATES_MAX);
    found
}

/// Merge or pull request links in `text`, in order of appearance.
fn review_urls(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let url = word.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')' | '<' | '>' | '[' | ']' | ',' | '.' | '"' | '\''
            )
        });
        let is_review_link = (url.starts_with("https://") || url.starts_with("http://"))
            && (url.contains("/merge_requests/") || url.contains("/pull/"));
        if is_review_link && !found.iter().any(|seen| seen == url) {
            found.push(url.to_string());
        }
    }
    found.truncate(CITED_MAX);
    found
}

/// `(full hash, "short subject (branches: ...)")` for each candidate that is a real commit here.
fn verified_commits(repo: &Path, text: &str) -> Vec<(String, String)> {
    sha_candidates(text)
        .into_iter()
        .filter_map(|candidate| {
            let full = git_stdout(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{candidate}^{{commit}}"),
                ],
            )?;
            let subject = git_stdout(repo, &["log", "-1", "--format=%h %s", &full])?;
            let branches = git_stdout(
                repo,
                &["branch", "--contains", &full, "--format=%(refname:short)"],
            )
            .unwrap_or_default()
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join(", ");
            let note = if branches.is_empty() {
                subject
            } else {
                format!("{subject} (branches: {branches})")
            };
            Some((full, note))
        })
        .take(CITED_MAX)
        .collect()
}

pub(super) fn gather(
    state: &StateDir,
    repo: &Path,
    session: &str,
    final_message: &dyn Fn() -> Option<String>,
) -> DoneEvidence {
    let since = crate::commands::ctx::sessions::load_record(
        state,
        &crate::commands::ctx::sessions::short_id(session),
    )
    .map(|record| record.started_at)
    .unwrap_or_else(|| super::now_secs().saturating_sub(FALLBACK_WINDOW_SECS));
    let mut lines = Vec::new();
    let worktrees = worktree_lines(repo, since);
    if !worktrees.is_empty() {
        lines.push(format!(
            "{WORKTREES_LABEL}commits since the session started (may include other sessions' work)\n{}",
            worktrees.join("\n")
        ));
    }
    let mut cited_keys = Vec::new();
    if let Some(message) = final_message().filter(|message| !message.trim().is_empty()) {
        let excerpt = crate::utils::truncate_bytes(
            message.split_whitespace().collect::<Vec<_>>().join(" "),
            Some(CLAIM_BYTES),
        );
        lines.push(format!("{CLAIM_LABEL}{excerpt}"));
        let commits = verified_commits(repo, &message);
        let urls = review_urls(&message);
        let mut cited: Vec<String> = commits
            .iter()
            .map(|(_, note)| format!("commit {note}"))
            .collect();
        cited.extend(urls.iter().map(|url| format!("link {url} (not checked)")));
        if !cited.is_empty() {
            lines.push(format!("{CITED_LABEL}{}", cited.join("; ")));
        }
        cited_keys.extend(commits.into_iter().map(|(full, _)| full));
        cited_keys.extend(urls);
        cited_keys.sort();
    }
    let fingerprint = crate::commands::workflow::engine::hash_bytes(
        format!("{}\n{}", worktrees.join("\n"), cited_keys.join("\n")).as_bytes(),
    );
    DoneEvidence {
        lines: lines.join("\n"),
        fingerprint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_worktrees_reads_path_and_branch_and_marks_a_detached_head() {
        let listing = "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /r/.claude/worktrees/w1\nHEAD def\nbranch refs/heads/fix/x\n\nworktree /r/d\nHEAD 123\ndetached\n";
        assert_eq!(
            parse_worktrees(listing),
            vec![
                (PathBuf::from("/r"), "main".to_string()),
                (
                    PathBuf::from("/r/.claude/worktrees/w1"),
                    "fix/x".to_string()
                ),
                (PathBuf::from("/r/d"), "(detached)".to_string()),
            ]
        );
    }

    #[test]
    fn sha_candidates_and_review_urls_pick_only_commit_like_and_review_link_tokens() {
        let text = "pushed 1a2b3c4d and 1a2b3c4d again, see (https://gitlab.example/g/p/-/merge_requests/12). deadbeefx plain";
        assert_eq!(sha_candidates(text), vec!["1a2b3c4d"]);
        assert_eq!(
            review_urls(text),
            vec!["https://gitlab.example/g/p/-/merge_requests/12".to_string()]
        );
    }
}
