//! Pure parsers for the `gh`/`glab` outputs the watch reads. Tolerant of
//! unknown fields; a body that is not the expected shape is an `Err` naming it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Check, CheckStatus, Comment, Lifecycle, Mergeability};
use crate::commands::ctx::window::parse_rfc3339_utc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forge {
    Github,
    Gitlab,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrRef {
    pub forge: Forge,
    pub host: String,
    /// `owner/repo` or the full GitLab group path.
    pub repo: String,
    pub number: u64,
}

/// One read of the PR/MR itself, before checks are attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct View {
    pub head_sha: Option<String>,
    pub lifecycle: Lifecycle,
    pub mergeability: Mergeability,
    pub author: Option<String>,
    pub comments: Vec<Comment>,
    /// GitLab: the head pipeline whose jobs are the checks.
    pub pipeline_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    NoCli,
    Auth,
    NotFound,
    RateLimited,
    Transient,
    UntrustedHost,
}

impl Failure {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::NoCli => "no_cli",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::Transient => "transient",
            Self::UntrustedHost => "untrusted_host",
        }
    }
}

/// `https://<host>/<repo>/pull/<n>` (GitHub; no port, no IP-literal host) or `https://<host>/<group>/<repo>/-/merge_requests/<n>` (GitLab).
pub(super) fn parse_url(url: &str) -> Option<PrRef> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !host.starts_with('-')
        && host.parse::<std::net::Ipv4Addr>().is_err();
    if !host_ok {
        return None;
    }
    let path = path.split(['?', '#']).next()?.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    let safe = |s: &&str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    if let Some(at) = segments.iter().position(|s| *s == "-")
        && segments.get(at + 1) == Some(&"merge_requests")
    {
        let number = segments.get(at + 2)?.parse().ok()?;
        let repo = &segments[..at];
        if repo.len() < 2 || !repo.iter().all(safe) {
            return None;
        }
        return Some(PrRef {
            forge: Forge::Gitlab,
            host: host.to_string(),
            repo: repo.join("/"),
            number,
        });
    }
    if segments.len() >= 4 && segments[2] == "pull" && safe(&segments[0]) && safe(&segments[1]) {
        return Some(PrRef {
            forge: Forge::Github,
            host: host.to_string(),
            repo: format!("{}/{}", segments[0], segments[1]),
            number: segments[3].parse().ok()?,
        });
    }
    None
}

/// The PR/MR URL a successful `gh pr create` / `glab mr create` printed to `stdout`, when `command` really invokes one (env prefixes and `&&`/`;`/`|` chains allowed; quoted, echoed or heredoc text never matches). Both CLIs print the URL alone on its own line; `parse_url` still gates it and the forge must match the CLI.
pub(crate) fn parse_created_url(command: &str, stdout: &str) -> Option<String> {
    let forge = created_forge(command)?;
    stdout.lines().map(str::trim).find_map(|line| {
        let pr = parse_url(line)?;
        (pr.forge == forge).then(|| line.to_string())
    })
}

/// The forge whose create command `command` invokes as a command word, if any.
fn created_forge(command: &str) -> Option<Forge> {
    command_segments(command).iter().find_map(|words| {
        let mut words = words.iter().map(String::as_str).peekable();
        while words
            .next_if(|w| is_env_assignment(w) || *w == "env")
            .is_some()
        {}
        let program = words.next()?.rsplit(['/', '\\']).next()?;
        let program = program.strip_suffix(".exe").unwrap_or(program);
        let noun = words.next()?;
        let verb = words.next()?;
        match (program, noun, verb) {
            ("gh", "pr", "create" | "new") => Some(Forge::Github),
            ("glab", "mr" | "merge-request", "create" | "new") => Some(Forge::Gitlab),
            _ => None,
        }
    })
}

fn is_env_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Splits a shell command into simple commands of unquoted words: separators `&&`, `||`, `;`, `|`, `&` and newlines split, quotes keep their text inside one word, and heredoc bodies are dropped.
fn command_segments(command: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut heredocs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        i += 1;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' && next.is_some_and(|n| "\"$`\\".contains(n)) {
                word.push(next.unwrap_or_default());
                i += 1;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                in_word = true;
            }
            '\\' if next.is_some_and(|n| " \"'$;&|\n".contains(n)) => {
                word.push(next.unwrap_or_default());
                in_word = true;
                i += 1;
            }
            '<' if next == Some('<') && chars.get(i + 1..i + 3) != Some(&['<', '<']) => {
                i += 1;
                if chars.get(i) == Some(&'-') {
                    i += 1;
                }
                while chars.get(i).is_some_and(|c| *c == ' ' || *c == '\t') {
                    i += 1;
                }
                let mut delim = String::new();
                while let Some(&d) = chars.get(i)
                    && !d.is_whitespace()
                    && !";&|<>()".contains(d)
                {
                    if d != '\'' && d != '"' && d != '\\' {
                        delim.push(d);
                    }
                    i += 1;
                }
                heredocs.push(delim);
                in_word = true;
                word.push_str("<<");
            }
            ' ' | '\t' | '\r' => flush_word(&mut segments, &mut word, &mut in_word),
            '\n' => {
                flush_word(&mut segments, &mut word, &mut in_word);
                segments.push(Vec::new());
                for delim in heredocs.drain(..) {
                    while i < chars.len() {
                        let end = chars[i..].iter().position(|c| *c == '\n');
                        let stop = end.map_or(chars.len(), |e| i + e);
                        let line: String = chars[i..stop].iter().collect();
                        i = (stop + 1).min(chars.len());
                        if line.trim() == delim {
                            break;
                        }
                    }
                }
            }
            ';' | '&' | '|' => {
                flush_word(&mut segments, &mut word, &mut in_word);
                if next == Some(c) {
                    i += 1;
                }
                segments.push(Vec::new());
            }
            _ => {
                word.push(c);
                in_word = true;
            }
        }
    }
    flush_word(&mut segments, &mut word, &mut in_word);
    segments
}

fn flush_word(segments: &mut [Vec<String>], word: &mut String, in_word: &mut bool) {
    if *in_word && let Some(last) = segments.last_mut() {
        last.push(std::mem::take(word));
    }
    *in_word = false;
}

/// The host of one git remote URL: `https://host/..`, `ssh://[user@]host[:port]/..` or scp-style `user@host:path`.
pub(super) fn remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    let authority = match url.split_once("://") {
        Some((_, rest)) => rest.split('/').next()?,
        None => url.split_once(':')?.0,
    };
    let host_port = authority.rsplit('@').next()?;
    if host_port.starts_with('[') {
        return None;
    }
    let host = host_port.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut at = value;
    for key in path {
        at = at.get(key)?;
    }
    at.as_str().filter(|s| !s.is_empty())
}

fn secs(value: &Value, key: &str) -> Option<u64> {
    parse_rfc3339_utc(value.get(key)?.as_str()?)
}

fn edited(at: u64, updated: Option<u64>) -> Option<u64> {
    updated.filter(|u| *u > at)
}

pub(super) fn gh_view(text: &str) -> Result<View, String> {
    let doc = json(text)?;
    let lifecycle = match str_at(&doc, &["state"]).ok_or("no state field")? {
        "MERGED" => Lifecycle::Merged,
        "CLOSED" => Lifecycle::Closed,
        _ => Lifecycle::Open,
    };
    let mergeability = match str_at(&doc, &["mergeable"]) {
        Some("CONFLICTING") => Mergeability::Conflicting,
        Some("MERGEABLE") => Mergeability::Mergeable,
        _ => Mergeability::Unknown,
    };
    let mut comments = Vec::new();
    for item in doc
        .get("comments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(id), Some(at)) = (str_at(item, &["id"]), secs(item, "createdAt")) else {
            continue;
        };
        comments.push(Comment {
            id: id.to_string(),
            author: str_at(item, &["author", "login"])
                .unwrap_or("ghost")
                .to_string(),
            at,
            edited_at: None,
            path: None,
            body: str_at(item, &["body"]).unwrap_or_default().to_string(),
            review_state: None,
            url: str_at(item, &["url"]).map(str::to_string),
        });
    }
    for item in doc
        .get("reviews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let state = str_at(item, &["state"]);
        let body = str_at(item, &["body"]).unwrap_or_default();
        // A draft has no submit time; an empty plain COMMENTED review carries nothing to act on.
        let (Some(id), Some(at)) = (str_at(item, &["id"]), secs(item, "submittedAt")) else {
            continue;
        };
        if body.is_empty() && state == Some("COMMENTED") {
            continue;
        }
        comments.push(Comment {
            id: id.to_string(),
            author: str_at(item, &["author", "login"])
                .unwrap_or("ghost")
                .to_string(),
            at,
            edited_at: None,
            path: None,
            body: body.to_string(),
            review_state: state.map(str::to_string),
            url: None,
        });
    }
    Ok(View {
        head_sha: str_at(&doc, &["headRefOid"]).map(str::to_string),
        lifecycle,
        mergeability,
        author: str_at(&doc, &["author", "login"]).map(str::to_string),
        comments,
        pipeline_id: None,
    })
}

pub(super) fn gh_checks(text: &str) -> Result<Vec<Check>, String> {
    // `gh pr checks` prints nothing on stdout when the PR has no checks.
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc = json(text)?;
    let items = doc.as_array().ok_or("checks are not an array")?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let status = match str_at(item, &["bucket"])? {
                "pass" => CheckStatus::Pass,
                "fail" => CheckStatus::Fail,
                "cancel" => CheckStatus::Cancelled,
                "skipping" => CheckStatus::Skipped,
                _ => CheckStatus::Pending,
            };
            Some(Check {
                name: str_at(item, &["name"])?.to_string(),
                status,
                required: false,
                url: str_at(item, &["link"]).map(str::to_string),
            })
        })
        .collect())
}

/// Marks every check in `all` whose name appears in `required`.
pub(super) fn mark_required(all: &mut [Check], required: &[Check]) {
    for check in all {
        check.required = required.iter().any(|r| r.name == check.name);
    }
}

pub(super) fn gh_inline_comments(text: &str) -> Result<Vec<Comment>, String> {
    let doc = json(text)?;
    let items = doc.as_array().ok_or("comments are not an array")?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let at = secs(item, "created_at")?;
            Some(Comment {
                id: format!("i-{}", item.get("id")?.as_u64()?),
                author: str_at(item, &["user", "login"])
                    .unwrap_or("ghost")
                    .to_string(),
                at,
                edited_at: edited(at, secs(item, "updated_at")),
                path: str_at(item, &["path"]).map(str::to_string),
                body: str_at(item, &["body"]).unwrap_or_default().to_string(),
                review_state: None,
                url: str_at(item, &["html_url"]).map(str::to_string),
            })
        })
        .collect())
}

pub(super) fn glab_view(text: &str) -> Result<View, String> {
    let doc = json(text)?;
    let lifecycle = match str_at(&doc, &["state"]).ok_or("no state field")? {
        "merged" => Lifecycle::Merged,
        "closed" => Lifecycle::Closed,
        _ => Lifecycle::Open,
    };
    let computing = matches!(
        str_at(&doc, &["detailed_merge_status"]),
        Some("checking" | "unchecked" | "preparing")
    );
    let mergeability = if doc.get("has_conflicts").and_then(Value::as_bool) == Some(true) {
        Mergeability::Conflicting
    } else if computing {
        Mergeability::Unknown
    } else {
        Mergeability::Mergeable
    };
    Ok(View {
        head_sha: str_at(&doc, &["sha"]).map(str::to_string),
        lifecycle,
        mergeability,
        author: str_at(&doc, &["author", "username"]).map(str::to_string),
        comments: Vec::new(),
        pipeline_id: doc
            .get("head_pipeline")
            .and_then(|p| p.get("id"))
            .and_then(Value::as_u64),
    })
}

pub(super) fn glab_jobs(text: &str) -> Result<Vec<Check>, String> {
    let doc = json(text)?;
    let items = doc.as_array().ok_or("jobs are not an array")?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let allowed = item.get("allow_failure").and_then(Value::as_bool) == Some(true);
            let status = match str_at(item, &["status"])? {
                "success" => CheckStatus::Pass,
                "failed" if allowed => CheckStatus::Skipped,
                "failed" => CheckStatus::Fail,
                "canceled" | "canceling" => CheckStatus::Cancelled,
                "skipped" | "manual" => CheckStatus::Skipped,
                _ => CheckStatus::Pending,
            };
            Some(Check {
                name: str_at(item, &["name"])?.to_string(),
                status,
                // The whole pipeline is the gate; jobs allowed to fail are advisory.
                required: !allowed,
                url: str_at(item, &["web_url"]).map(str::to_string),
            })
        })
        .collect())
}

pub(super) fn glab_discussions(text: &str) -> Result<Vec<Comment>, String> {
    let doc = json(text)?;
    let threads = doc.as_array().ok_or("discussions are not an array")?;
    Ok(threads
        .iter()
        .flat_map(|thread| {
            thread
                .get("notes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|note| note.get("system").and_then(Value::as_bool) != Some(true))
        .filter_map(|note| {
            let at = secs(note, "created_at")?;
            Some(Comment {
                id: format!("n-{}", note.get("id")?.as_u64()?),
                author: str_at(note, &["author", "username"])
                    .unwrap_or("ghost")
                    .to_string(),
                at,
                edited_at: edited(at, secs(note, "updated_at")),
                path: str_at(note, &["position", "new_path"]).map(str::to_string),
                body: str_at(note, &["body"]).unwrap_or_default().to_string(),
                review_state: None,
                url: None,
            })
        })
        .collect())
}

/// Buckets CLI stderr or an API error body into what the poller should do about it.
pub(super) fn classify_failure(text: &str) -> Failure {
    let lower = text.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    // A bare code also matches PR numbers and URLs (`/pulls/1404/`); only the CLIs' status phrasings count.
    let status = |code: &str| {
        [
            "http ",
            "http/1.1 ",
            "http/2 ",
            "status ",
            "status: ",
            "status\":\"",
            "status\": ",
        ]
        .iter()
        .any(|prefix| lower.contains(&format!("{prefix}{code}")))
    };
    if has(&["rate limit", "too many requests"]) || status("429") || status("403") {
        return Failure::RateLimited;
    }
    if has(&["not found", "could not resolve"]) || status("404") {
        return Failure::NotFound;
    }
    if has(&[
        "auth login",
        "bad credentials",
        "not logged",
        "authentication",
        "unauthorized",
    ]) || status("401")
    {
        return Failure::Auth;
    }
    Failure::Transient
}

#[cfg(test)]
mod tests {
    use super::*;

    const GH_VIEW: &str = include_str!("../../../../tests/fixtures/pr-watch/gh-pr-view.json");
    const GH_VIEW_MERGED: &str =
        include_str!("../../../../tests/fixtures/pr-watch/gh-pr-view-merged-minimal.json");
    const GH_CHECKS: &str = include_str!("../../../../tests/fixtures/pr-watch/gh-checks.json");
    const GH_CHECKS_REQUIRED: &str =
        include_str!("../../../../tests/fixtures/pr-watch/gh-checks-required.json");
    const GH_CHECKS_EMPTY: &str =
        include_str!("../../../../tests/fixtures/pr-watch/gh-checks-empty.json");
    const GH_INLINE: &str =
        include_str!("../../../../tests/fixtures/pr-watch/gh-inline-comments.json");
    const GH_404: &str = include_str!("../../../../tests/fixtures/pr-watch/gh-404.json");
    const GLAB_VIEW: &str = include_str!("../../../../tests/fixtures/pr-watch/glab-mr-view.json");
    const GLAB_JOBS: &str = include_str!("../../../../tests/fixtures/pr-watch/glab-jobs.json");
    const GLAB_DISCUSSIONS: &str =
        include_str!("../../../../tests/fixtures/pr-watch/glab-discussions.json");
    const GLAB_404: &str = include_str!("../../../../tests/fixtures/pr-watch/glab-404.json");

    #[test]
    fn created_urls_come_from_real_create_commands_only() {
        const GH: &str = "https://github.com/acme/widgets/pull/12";
        const GL: &str = "https://gitlab.example.com/group/proj/-/merge_requests/7";
        let gh_out = format!("{GH}\n");
        let gl_out = format!("!7 Fix it\n{GL}\n");
        for (cmd, out, want) in [
            ("gh pr create --fill", gh_out.as_str(), Some(GH)),
            ("gh pr new -t 'a && b' -b \"x\"", gh_out.as_str(), Some(GH)),
            ("glab mr create -f --yes", gl_out.as_str(), Some(GL)),
            ("cd x && gh pr create --fill", gh_out.as_str(), Some(GH)),
            ("git push; gh pr create", gh_out.as_str(), Some(GH)),
            (
                "GH_HOST=github.com GH_TOKEN=x gh pr create",
                gh_out.as_str(),
                Some(GH),
            ),
            (
                "env FOO=1 /opt/homebrew/bin/gh pr create",
                gh_out.as_str(),
                Some(GH),
            ),
            (
                "glab mr create",
                format!("!7 Fix it (feat)\n {GL}\n").as_str(),
                Some(GL),
            ),
            ("echo gh pr create", gh_out.as_str(), None),
            ("echo \"gh pr create\"", gh_out.as_str(), None),
            (
                "echo 'x' && echo \"a; gh pr create\"",
                gh_out.as_str(),
                None,
            ),
            ("cat <<EOF\ngh pr create\nEOF", gh_out.as_str(), None),
            (
                "cat <<'EOF' > f.md\ngh pr create\nEOF\nls",
                gh_out.as_str(),
                None,
            ),
            ("cat <<EOF\nx\nEOF\ngh pr create", gh_out.as_str(), Some(GH)),
            ("gh pr view 12", gh_out.as_str(), None),
            ("gh pr create", "created nothing\n", None),
            (
                "gh pr create",
                "https://github.com:8443/acme/widgets/pull/12\n",
                None,
            ),
            (
                "gh pr create",
                "https://127.0.0.1/acme/widgets/pull/12\n",
                None,
            ),
            ("gh pr create", gl_out.as_str(), None),
            ("glab mr create", gh_out.as_str(), None),
        ] {
            assert_eq!(parse_created_url(cmd, out).as_deref(), want, "{cmd:?}");
        }
    }

    #[test]
    fn urls_parse_for_github_gitlab_and_enterprise_hosts() {
        let gh = parse_url("https://github.com/acme/widgets/pull/12").expect("github");
        assert_eq!(
            (gh.forge, gh.host.as_str(), gh.repo.as_str(), gh.number),
            (Forge::Github, "github.com", "acme/widgets", 12)
        );
        let ghe = parse_url("https://ghe.corp.example/acme/widgets/pull/3/files").expect("ghe");
        assert_eq!(
            (ghe.forge, ghe.host.as_str(), ghe.number),
            (Forge::Github, "ghe.corp.example", 3)
        );
        let gl = parse_url("https://gitlab.example.com/group/sub/proj/-/merge_requests/7")
            .expect("gitlab");
        assert_eq!(
            (gl.forge, gl.repo.as_str(), gl.number),
            (Forge::Gitlab, "group/sub/proj", 7)
        );
        for bad in [
            "",
            "http://github.com/a/b/pull/1",
            "https://github.com/a/b/issues/1",
            "https://github.com/a/b/pull/x",
            "https://-evil/a/b/pull/1",
            "--flag",
            "https://github.com/a/b/pull/1 --extra",
            "https://github.com:8443/a/b/pull/1",
            "https://127.0.0.1/a/b/pull/1",
            "https://[::1]/a/b/pull/1",
            "https://[::1]:443/a/b/pull/1",
        ] {
            assert!(parse_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_github_view_yields_head_state_author_and_both_comment_kinds() {
        let view = gh_view(GH_VIEW).expect("parse");
        assert_eq!(
            view.head_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(view.lifecycle, Lifecycle::Open);
        assert_eq!(view.mergeability, Mergeability::Conflicting);
        assert_eq!(view.author.as_deref(), Some("Glubiz"));
        let ids: Vec<&str> = view.comments.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["IC_kwDOAAA1", "PRR_kwDOAAA2"],
            "empty COMMENTED and PENDING reviews are not news"
        );
        assert_eq!(view.comments[0].author, "reviewer-a");
        assert_eq!(view.comments[0].at, 1_794_218_400);
        assert_eq!(
            view.comments[1].review_state.as_deref(),
            Some("CHANGES_REQUESTED")
        );
    }

    #[test]
    fn a_minimal_merged_view_with_unknown_fields_still_parses() {
        let view = gh_view(GH_VIEW_MERGED).expect("parse");
        assert_eq!(view.lifecycle, Lifecycle::Merged);
        assert_eq!(view.mergeability, Mergeability::Unknown);
        assert_eq!(view.head_sha, None, "an empty head is not a head");
        assert!(view.comments.is_empty());
    }

    #[test]
    fn a_404_body_or_empty_output_is_not_a_view() {
        assert!(gh_view(GH_404).is_err());
        assert!(gh_view("").is_err());
        assert!(gh_view("not json").is_err());
        assert!(glab_view(GLAB_404).is_err());
    }

    #[test]
    fn github_checks_map_buckets_and_leave_required_to_the_second_read() {
        let checks = gh_checks(GH_CHECKS).expect("parse");
        let statuses: Vec<(&str, CheckStatus)> =
            checks.iter().map(|c| (c.name.as_str(), c.status)).collect();
        assert_eq!(
            statuses,
            [
                ("build", CheckStatus::Pass),
                ("test", CheckStatus::Fail),
                ("lint", CheckStatus::Pending),
                ("docs", CheckStatus::Skipped),
                ("deploy", CheckStatus::Cancelled),
            ]
        );
        assert!(checks.iter().all(|c| !c.required));
        assert_eq!(checks[2].url, None, "an empty link is no link");
        let mut checks = checks;
        mark_required(
            &mut checks,
            &gh_checks(GH_CHECKS_REQUIRED).expect("required"),
        );
        let required: Vec<&str> = checks
            .iter()
            .filter(|c| c.required)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(required, ["build"]);
    }

    #[test]
    fn empty_checks_parse_as_an_empty_list_not_an_error() {
        assert!(gh_checks(GH_CHECKS_EMPTY).expect("empty array").is_empty());
        assert!(gh_checks("").expect("no output means no checks").is_empty());
        assert!(gh_checks("{\"message\":\"x\"}").is_err());
    }

    #[test]
    fn inline_review_comments_carry_path_edit_time_and_survive_a_deleted_user() {
        let comments = gh_inline_comments(GH_INLINE).expect("parse");
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "i-9001");
        assert_eq!(comments[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(comments[0].edited_at, Some(comments[0].at + 300));
        assert_eq!(comments[1].author, "ghost");
        assert_eq!(comments[1].edited_at, None, "same stamp is not an edit");
    }

    #[test]
    fn a_gitlab_view_reads_sha_state_pipeline_and_computing_mergeability() {
        let view = glab_view(GLAB_VIEW).expect("parse");
        assert_eq!(
            view.head_sha.as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba98")
        );
        assert_eq!(view.lifecycle, Lifecycle::Open);
        assert_eq!(view.mergeability, Mergeability::Unknown);
        assert_eq!(view.author.as_deref(), Some("josj"));
        assert_eq!(view.pipeline_id, Some(4242));
        let conflicted = GLAB_VIEW.replace("\"has_conflicts\": false", "\"has_conflicts\": true");
        assert_eq!(
            glab_view(&conflicted).expect("parse").mergeability,
            Mergeability::Conflicting
        );
        let merged = GLAB_VIEW.replace("\"opened\"", "\"merged\"");
        assert_eq!(
            glab_view(&merged).expect("parse").lifecycle,
            Lifecycle::Merged
        );
        let settled = GLAB_VIEW.replace("checking", "mergeable");
        assert_eq!(
            glab_view(&settled).expect("parse").mergeability,
            Mergeability::Mergeable
        );
    }

    #[test]
    fn gitlab_jobs_gate_on_the_pipeline_except_allowed_failures() {
        let jobs = glab_jobs(GLAB_JOBS).expect("parse");
        let rows: Vec<(&str, CheckStatus, bool)> = jobs
            .iter()
            .map(|c| (c.name.as_str(), c.status, c.required))
            .collect();
        assert_eq!(
            rows,
            [
                ("build", CheckStatus::Pass, true),
                ("test", CheckStatus::Fail, true),
                ("e2e", CheckStatus::Skipped, false),
                ("deploy", CheckStatus::Skipped, false),
                ("package", CheckStatus::Pending, true),
                ("scan", CheckStatus::Cancelled, true),
            ]
        );
    }

    #[test]
    fn gitlab_discussions_drop_system_notes_and_flatten_threads() {
        let comments = glab_discussions(GLAB_DISCUSSIONS).expect("parse");
        let ids: Vec<&str> = comments.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["n-501", "n-503"]);
        assert_eq!(comments[0].path.as_deref(), Some("src/a.rs"));
        assert_eq!(comments[0].edited_at, Some(comments[0].at + 1800));
        assert_eq!(comments[1].author, "reviewer-b");
    }

    #[test]
    fn failures_are_classified_from_cli_text_and_error_bodies() {
        assert_eq!(classify_failure(GH_404), Failure::NotFound);
        assert_eq!(classify_failure(GLAB_404), Failure::NotFound);
        assert_eq!(
            classify_failure("GraphQL: Could not resolve to a PullRequest"),
            Failure::NotFound
        );
        assert_eq!(
            classify_failure("API rate limit exceeded for user"),
            Failure::RateLimited
        );
        assert_eq!(
            classify_failure("HTTP 429: Too Many Requests"),
            Failure::RateLimited
        );
        assert_eq!(
            classify_failure("To get started with GitHub CLI, please run:  gh auth login"),
            Failure::Auth
        );
        assert_eq!(classify_failure("HTTP 401: Bad credentials"), Failure::Auth);
        assert_eq!(
            classify_failure("connection reset by peer"),
            Failure::Transient
        );
    }

    #[test]
    fn a_5xx_naming_a_pull_number_with_a_status_code_inside_is_transient() {
        assert_eq!(
            classify_failure(
                "HTTP 502: Bad Gateway (https://api.github.com/repos/a/b/pulls/1404/comments)"
            ),
            Failure::Transient
        );
        assert_eq!(
            classify_failure("glab: GET https://h/api/v4/projects/g%2Fp/merge_requests/403: 500"),
            Failure::Transient
        );
        assert_eq!(classify_failure("HTTP 404: Not Found"), Failure::NotFound);
        assert_eq!(
            classify_failure("HTTP 403: Forbidden"),
            Failure::RateLimited
        );
        assert_eq!(classify_failure("HTTP 401"), Failure::Auth);
    }

    #[test]
    fn remote_hosts_come_from_https_ssh_and_scp_style_urls() {
        for (url, host) in [
            ("https://GitHub.com/a/b.git", Some("github.com")),
            (
                "https://user:tok@gitlab.example.com/g/p.git",
                Some("gitlab.example.com"),
            ),
            (
                "ssh://git@ghe.corp.example:2222/a/b.git",
                Some("ghe.corp.example"),
            ),
            ("git@gitlab.cego.dk:group/proj.git", Some("gitlab.cego.dk")),
            ("/srv/git/local.git", None),
        ] {
            assert_eq!(remote_host(url).as_deref(), host, "{url}");
        }
    }
}
