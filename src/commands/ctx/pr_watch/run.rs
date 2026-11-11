//! `zirv ctx pr watch|unwatch|list|poll`: registration, the `gh`/`glab`
//! reads (argv arrays, no shell, 10 s timeout, the CLIs' own auth) and wake
//! delivery as mail. `poll` is a manual sweep for one session; failures are
//! recorded on the watch and never mailed.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::parse::{self, Failure, Forge, PrRef, View};
use super::store::{self, LastError, WatchRecord};
use super::{Check, Lifecycle, Limits, Snapshot, Wake, WatchState, evaluate, render};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::adapters::{AGENT_ENV, SESSION_ENV};
use crate::commands::ctx::config::{CtxConfig, EnvLookup, env_from_process};
use crate::commands::ctx::mail::{self, Message};
use crate::commands::ctx::sessions::short_id;
use crate::commands::ctx::state::{StateDir, now_secs, repo_slug};
use crate::commands::ctx::worktree;

const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const SENDER: &str = "zirv-pr-watch";
/// Consecutive failed reads before a watch stops polling.
const MAX_FAILURES: u32 = 8;
/// Same URL answered "not found" this many times in a row: the PR is gone.
const MAX_NOT_FOUND: u32 = 2;

#[derive(Debug, clap::Args)]
pub struct PrArgs {
    #[command(subcommand)]
    pub command: PrVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum PrVerb {
    /// Watch a pull/merge request for this session.
    Watch(UrlArgs),
    /// Stop watching a pull/merge request.
    Unwatch(UrlArgs),
    /// List this session's watches and their last error.
    List(SessionArgs),
    /// Read every watch of this session once and mail what changed.
    Poll(SessionArgs),
}

#[derive(Debug, clap::Args)]
pub struct UrlArgs {
    /// The pull/merge request URL.
    pub url: String,
    /// Act for this session short id instead of the calling session.
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct SessionArgs {
    /// Act for this session short id instead of the calling session.
    #[arg(long)]
    pub session: Option<String>,
}

pub fn run<W: Write>(args: &PrArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    run_with(args, w, &repo, &env_from_process())
}

pub(super) fn run_with<W: Write>(
    args: &PrArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let (verb, session) = match &args.command {
        PrVerb::Watch(a) => ("watch", &a.session),
        PrVerb::Unwatch(a) => ("unwatch", &a.session),
        PrVerb::List(a) => ("list", &a.session),
        PrVerb::Poll(a) => ("poll", &a.session),
    };
    if matches!(verb, "watch" | "poll") && !cfg.pr_watch.enabled {
        return Err(format!(
            "zirv ctx pr {verb}: PR watching is disabled; set `[pr_watch] enabled = true` in ~/.zirv/ctx.toml or ZIRV_CTX_PR_WATCH=true"
        )
        .into());
    }
    let owner = session
        .as_deref()
        .or(env(SESSION_ENV).as_deref())
        .map(short_id)
        .filter(|short| !short.is_empty())
        .ok_or_else(|| format!("zirv ctx pr {verb}: no session; run inside a zirv session or pass --session <short>"))?;
    let state = StateDir::resolve(env)?;

    match &args.command {
        PrVerb::Watch(a) => {
            let Some(pr) = parse::parse_url(&a.url) else {
                return Err(format!("zirv ctx pr watch: '{}' is not a GitHub pull request or GitLab merge request URL (https, no port, no IP address)", a.url).into());
            };
            if let Err(allowed) = trusted_host(repo, &pr.host) {
                return Err(format!("zirv ctx pr watch: host '{}' is not a git remote host of this repository; allowed: {allowed}", pr.host).into());
            }
            if store::load(&state, &owner, &a.url).is_some() {
                writeln!(w, "zirv ctx pr watch: already watching {}", a.url)?;
                return Ok(0);
            }
            let now = now_secs();
            store::save(
                &state,
                &WatchRecord {
                    url: a.url.clone(),
                    owner,
                    repo: repo.to_path_buf(),
                    repo_slug: repo_slug(repo),
                    agent: env(AGENT_ENV).unwrap_or_else(|| "any".into()),
                    viewer: None,
                    state: WatchState::new(now),
                    registered_at: now,
                    last_polled: 0,
                    wake_seq: 0,
                    last_error: None,
                    consecutive_failures: 0,
                    not_found: 0,
                    stopped: None,
                },
            )?;
            writeln!(
                w,
                "zirv ctx pr watch: watching {} #{} on {}",
                pr.repo, pr.number, pr.host
            )?;
            Ok(0)
        }
        PrVerb::Unwatch(a) => {
            let removed = store::remove(&state, &owner, &a.url);
            writeln!(
                w,
                "zirv ctx pr unwatch: {}",
                if removed {
                    "stopped watching"
                } else {
                    "was not watching"
                }
            )?;
            Ok(0)
        }
        PrVerb::List(_) => {
            for rec in store::list(&state, &owner) {
                writeln!(
                    w,
                    "{}\thead={}\twakes={}\tlast_polled={}\terror={}\tstopped={}",
                    rec.url,
                    rec.state.head_sha.chars().take(7).collect::<String>(),
                    rec.wake_seq,
                    rec.last_polled,
                    rec.last_error.as_ref().map_or("-", |e| e.kind.as_str()),
                    rec.stopped.as_deref().unwrap_or("-"),
                )?;
            }
            Ok(0)
        }
        PrVerb::Poll(_) => {
            if !cfg.mail.enabled {
                return Err("zirv ctx pr poll: mail is disabled (mail.enabled = false); wakes are delivered as mail".into());
            }
            poll_session(&state, &cfg, env, &owner, &mut |url, line| {
                writeln!(w, "{url}: {line}")?;
                Ok(())
            })?;
            Ok(0)
        }
    }
}

/// Reads every watch of `owner` once, one at a time, reporting `(url, outcome)` to `each` as it goes.
pub(super) fn poll_session(
    state: &StateDir,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    owner: &str,
    each: &mut dyn FnMut(&str, &str) -> CtxResult<()>,
) -> CtxResult<()> {
    let limits = Limits {
        no_checks_secs: cfg.pr_watch.no_checks_minutes * 60,
        max_wakes: cfg.pr_watch.max_wakes,
    };
    for rec in store::list(state, owner) {
        let line = poll_one(state, cfg, env, owner, &rec.url, &limits)?;
        each(&rec.url, &line)?;
    }
    Ok(())
}

/// Ok when `host` equals a host of one of `repo`'s git remotes; otherwise Err with the allowed hosts.
fn trusted_host(repo: &Path, host: &str) -> Result<(), String> {
    let remotes = worktree::run_git(repo, &["remote", "-v"]).unwrap_or_default();
    let mut hosts: Vec<String> = remotes
        .lines()
        .filter_map(|line| parse::remote_host(line.split_whitespace().nth(1)?))
        .collect();
    hosts.sort();
    hosts.dedup();
    if hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return Ok(());
    }
    Err(if hosts.is_empty() {
        "(none; the repository has no git remotes)".into()
    } else {
        hosts.join(", ")
    })
}

/// Reads one watch under its try-lock, evaluates it and delivers wakes. Read failures are recorded, not mailed.
fn poll_one(
    state: &StateDir,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    owner: &str,
    url: &str,
    limits: &Limits,
) -> CtxResult<String> {
    let Ok(_lock) = store::try_lock(state, owner, url) else {
        return Ok("busy (another poller holds this watch)".into());
    };
    // Re-read under the lock: another poller may have advanced it since the listing.
    let Some(mut rec) = store::load(state, owner, url) else {
        return Ok("gone".into());
    };
    if let Some(why) = &rec.stopped {
        return Ok(format!("stopped ({why})"));
    }
    let now = now_secs();
    rec.last_polled = now;
    let snap = match fetch(env, &rec) {
        Ok(snap) => snap,
        Err(kind) => {
            rec.last_error = Some(LastError { kind, at: now });
            rec.consecutive_failures += 1;
            rec.not_found = if kind == Failure::NotFound {
                rec.not_found + 1
            } else {
                0
            };
            if rec.not_found >= MAX_NOT_FOUND {
                rec.stopped = Some("not_found".into());
            } else if rec.consecutive_failures >= MAX_FAILURES {
                rec.stopped = Some("failures".into());
            }
            store::save(state, &rec)?;
            return Ok(format!("error: {}", kind.as_str()));
        }
    };
    rec.last_error = None;
    rec.consecutive_failures = 0;
    rec.not_found = 0;
    rec.viewer = snap.viewer.clone().or(rec.viewer);
    let report = evaluate(&rec.state, &snap, now, limits);
    rec.state = report.next.clone();
    if report.wakes.is_empty() {
        store::save(state, &rec)?;
        return Ok("no change".into());
    }
    let ended = report.wakes.iter().any(|w| matches!(w, Wake::Ended(_)));
    if ended {
        rec.stopped = Some("ended".into());
    } else if report.exhausted {
        rec.stopped = Some("wake_cap".into());
    }
    // Persist the sequence before the mail: a crash loses one wake rather than repeating it.
    rec.wake_seq += 1;
    store::save(state, &rec)?;
    let body = format!(
        "[pr-watch wake #{}]\n{}",
        rec.wake_seq,
        render(&rec.url, &rec.state.head_sha, &report)
    );
    let msg = Message {
        from_session: SENDER.into(),
        from_agent: SENDER.into(),
        to: rec.agent.clone(),
        to_session: Some(rec.owner.clone()),
        sent: now,
        body,
    };
    mail::store_to(state, &rec.repo_slug, &rec.repo_slug, &msg, cfg)?;
    if ended {
        store::remove(state, &rec.owner, &rec.url);
    }
    Ok(format!("{} wake(s) mailed", report.wakes.len()))
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `program` with an argv array (no shell); the binary is looked up on the injected `PATH`.
fn cli(
    env: EnvLookup<'_>,
    program: &str,
    args: &[&str],
    host_env: &[(&str, &str)],
) -> Result<Out, Failure> {
    let path = env("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path)
        .flat_map(|dir| [dir.join(program), dir.join(format!("{program}.exe"))])
        .find(|candidate| candidate.is_file())
        .ok_or(Failure::NoCli)?;
    let mut command = Command::new(found);
    command.args(args).env("PATH", &path);
    for (key, value) in host_env {
        command.env(key, value);
    }
    let output = crate::utils::run_with_timeout(&mut command, CLI_TIMEOUT)
        .map_err(|_| Failure::Transient)?;
    Ok(Out {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Stdout of a command that must succeed; otherwise the failure its output describes.
fn must(out: Out) -> Result<String, Failure> {
    if out.ok {
        return Ok(out.stdout);
    }
    Err(parse::classify_failure(&format!(
        "{} {}",
        out.stdout, out.stderr
    )))
}

fn fetch(env: EnvLookup<'_>, rec: &WatchRecord) -> Result<Snapshot, Failure> {
    let pr: PrRef = parse::parse_url(&rec.url).ok_or(Failure::UntrustedHost)?;
    // The store can be edited: re-check before any CLI call carries the operator's token.
    trusted_host(&rec.repo, &pr.host).map_err(|_| Failure::UntrustedHost)?;
    let (view, checks, viewer) = match pr.forge {
        Forge::Github => fetch_github(env, rec, &pr)?,
        Forge::Gitlab => fetch_gitlab(env, rec, &pr)?,
    };
    let ci_config = match pr.forge {
        Forge::Github => rec.repo.join(".github").join("workflows").is_dir(),
        Forge::Gitlab => rec.repo.join(".gitlab-ci.yml").is_file(),
    };
    Ok(Snapshot {
        head_sha: view.head_sha,
        checks,
        mergeability: view.mergeability,
        comments: view.comments,
        viewer: viewer.or_else(|| rec.viewer.clone()),
        author: view.author,
        lifecycle: view.lifecycle,
        checks_always_report: ci_config,
    })
}

type Fetched = (View, Vec<Check>, Option<String>);

fn fetch_github(env: EnvLookup<'_>, rec: &WatchRecord, pr: &PrRef) -> Result<Fetched, Failure> {
    let host = [("GH_HOST", pr.host.as_str())];
    let gh = |args: &[&str]| cli(env, "gh", args, &host);
    let mut view = parse::gh_view(&must(gh(&[
        "pr",
        "view",
        &rec.url,
        "--json",
        "headRefOid,state,mergeable,mergeStateStatus,author,reviews,comments,url",
    ])?)?)
    .map_err(|_| Failure::Transient)?;
    if view.lifecycle != Lifecycle::Open {
        return Ok((view, Vec::new(), None));
    }
    // `gh pr checks` exits 1 when a check failed and 8 while pending; the JSON is still data.
    let checks_of = |extra: &[&str]| -> Result<Vec<Check>, Failure> {
        let mut args = vec![
            "pr",
            "checks",
            rec.url.as_str(),
            "--json",
            "name,state,bucket,link",
        ];
        args.extend_from_slice(extra);
        let out = gh(&args)?;
        let none_reported =
            out.stderr.contains("no checks reported") || out.stderr.contains("no required checks");
        if !out.ok && !out.stdout.trim_start().starts_with('[') && !none_reported {
            return Err(parse::classify_failure(&format!(
                "{} {}",
                out.stdout, out.stderr
            )));
        }
        parse::gh_checks(&out.stdout).map_err(|_| Failure::Transient)
    };
    let mut checks = checks_of(&[])?;
    if !checks.is_empty() {
        parse::mark_required(&mut checks, &checks_of(&["--required"])?);
    }
    let endpoint = format!(
        "repos/{}/pulls/{}/comments?per_page=100",
        pr.repo, pr.number
    );
    let inline = parse::gh_inline_comments(&must(gh(&["api", &endpoint])?)?)
        .map_err(|_| Failure::Transient)?;
    view.comments.extend(inline);
    let viewer = match &rec.viewer {
        Some(_) => None,
        None => Some(
            must(gh(&["api", "user", "--jq", ".login"])?)?
                .trim()
                .to_string(),
        )
        .filter(|v| !v.is_empty()),
    };
    Ok((view, checks, viewer))
}

fn fetch_gitlab(env: EnvLookup<'_>, rec: &WatchRecord, pr: &PrRef) -> Result<Fetched, Failure> {
    let host = [("GITLAB_HOST", pr.host.as_str())];
    let glab = |args: &[&str]| cli(env, "glab", args, &host);
    let number = pr.number.to_string();
    let view = parse::glab_view(&must(glab(&[
        "mr", "view", &number, "-R", &pr.repo, "-F", "json",
    ])?)?)
    .map_err(|_| Failure::Transient)?;
    if view.lifecycle != Lifecycle::Open {
        return Ok((view, Vec::new(), None));
    }
    let project = pr.repo.replace('/', "%2F");
    let discussions =
        format!("projects/{project}/merge_requests/{number}/discussions?per_page=100");
    let mut view = view;
    view.comments = parse::glab_discussions(&must(glab(&["api", &discussions])?)?)
        .map_err(|_| Failure::Transient)?;
    let checks = match view.pipeline_id {
        Some(id) => {
            let jobs = format!("projects/{project}/pipelines/{id}/jobs?per_page=100");
            parse::glab_jobs(&must(glab(&["api", &jobs])?)?).map_err(|_| Failure::Transient)?
        }
        None => Vec::new(),
    };
    let viewer = match &rec.viewer {
        Some(_) => None,
        None => {
            let user: serde_json::Value = serde_json::from_str(&must(glab(&["api", "user"])?)?)
                .map_err(|_| Failure::Transient)?;
            user.get("username")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
    };
    Ok((view, checks, viewer))
}

#[cfg(test)]
#[cfg(unix)]
pub(super) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    pub(in super::super) const GH_URL: &str = "https://github.com/acme/widgets/pull/12";
    const GL_URL: &str = "https://gitlab.example.com/group/sub/proj/-/merge_requests/7";
    pub(in super::super) const OWNER: &str = "abcd1234";

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pr-watch")
    }

    pub(in super::super) struct Rig {
        pub(in super::super) _root: tempfile::TempDir,
        pub(in super::super) bin: PathBuf,
        state_root: PathBuf,
        pub(in super::super) repo: PathBuf,
        pub(in super::super) env: HashMap<String, String>,
        _home: crate::commands::ctx::testenv::HomeGuard,
    }

    impl Rig {
        pub(in super::super) fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let home = root.path().join("home");
            let bin = root.path().join("bin");
            let state_root = root.path().join("state");
            let repo = root.path().join("repo");
            for dir in [&home, &bin, &repo] {
                std::fs::create_dir_all(dir).expect("mkdir");
            }
            worktree::tests::init_repo(&repo);
            for (name, url) in [
                ("origin", "https://github.com/acme/widgets.git"),
                ("lab", "https://gitlab.example.com/group/sub/proj.git"),
            ] {
                assert!(worktree::tests::run(&repo, &["remote", "add", name, url]));
            }
            let env = HashMap::from([
                ("PATH".to_string(), bin.display().to_string()),
                (
                    "ZIRV_CTX_STATE_DIR".to_string(),
                    state_root.display().to_string(),
                ),
                ("ZIRV_CTX_PR_WATCH".to_string(), "true".to_string()),
            ]);
            Self {
                _home: crate::commands::ctx::testenv::HomeGuard::set(&home),
                _root: root,
                bin,
                state_root,
                repo,
                env,
            }
        }

        /// A fake CLI on the injected PATH: `body` is a sh `case` over "$1 $2".
        pub(in super::super) fn shim(&self, name: &str, body: &str) {
            let path = self.bin.join(name);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nPATH=\"$PATH:/usr/bin:/bin\"\nF={}\necho \"GH_HOST=${{GH_HOST-unset}}\" >> \"{}.env\"\ncase \"$1 $2\" in\n{body}\nesac\n",
                    fixtures().display(),
                    path.display()
                ),
            )
            .expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        pub(in super::super) fn gh_shim(&self, view: &str) {
            self.shim(
                "gh",
                &format!(
                    "\"pr view\") cat \"$F/{view}\" ;;\n\
                     \"pr checks\") case \"$*\" in *--required*) cat \"$F/gh-checks-required.json\" ;; *) cat \"$F/gh-checks.json\"; exit 1 ;; esac ;;\n\
                     \"api user\") echo me ;;\n\
                     api*) cat \"$F/gh-inline-comments.json\" ;;\n\
                     *) echo \"unexpected $*\" >&2; exit 9 ;;"
                ),
            );
        }

        pub(in super::super) fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
            |key| self.env.get(key).cloned()
        }

        pub(in super::super) fn state(&self) -> StateDir {
            StateDir::from_root(self.state_root.clone())
        }

        fn pr(&self, args: PrVerb) -> CtxResult<String> {
            let mut out = Vec::new();
            run_with(
                &PrArgs { command: args },
                &mut out,
                &self.repo,
                &self.lookup(),
            )?;
            Ok(String::from_utf8(out).expect("utf8"))
        }

        fn url_args(url: &str) -> UrlArgs {
            UrlArgs {
                url: url.into(),
                session: Some(OWNER.into()),
            }
        }

        fn session_args() -> SessionArgs {
            SessionArgs {
                session: Some(OWNER.into()),
            }
        }

        pub(in super::super) fn watch(&self, url: &str) {
            self.pr(PrVerb::Watch(Self::url_args(url))).expect("watch");
            // Fixture comments predate "now": make them news.
            let mut rec = store::load(&self.state(), OWNER, url).expect("record");
            rec.state.remarks_through = 0;
            store::save(&self.state(), &rec).expect("save");
        }

        pub(in super::super) fn poll(&self) -> String {
            self.pr(PrVerb::Poll(Self::session_args())).expect("poll")
        }

        fn poll_mailed(&self) {
            let said = self.poll();
            assert!(said.contains("wake(s) mailed"), "{said}");
        }

        pub(in super::super) fn mails(&self) -> Vec<(PathBuf, Message)> {
            mail::list(&self.state(), &repo_slug(&self.repo), None, Some(OWNER)).unwrap_or_default()
        }
    }

    #[test]
    fn watch_and_poll_refuse_while_the_feature_is_disabled() {
        let mut rig = Rig::new();
        rig.env.insert("ZIRV_CTX_PR_WATCH".into(), "false".into());
        for verb in [
            PrVerb::Watch(Rig::url_args(GH_URL)),
            PrVerb::Poll(Rig::session_args()),
        ] {
            let err = rig.pr(verb).expect_err("disabled").to_string();
            assert!(
                err.contains("disabled") && err.contains("ZIRV_CTX_PR_WATCH"),
                "{err}"
            );
        }
        assert!(rig.pr(PrVerb::List(Rig::session_args())).is_ok());
    }

    #[test]
    fn watch_registers_lists_and_unwatches_and_rejects_foreign_urls() {
        let rig = Rig::new();
        let said = rig.pr(PrVerb::Watch(Rig::url_args(GH_URL))).expect("watch");
        assert!(said.contains("acme/widgets #12"), "{said}");
        let listing = rig.pr(PrVerb::List(Rig::session_args())).expect("list");
        assert!(listing.starts_with(GH_URL), "{listing}");
        let again = rig.pr(PrVerb::Watch(Rig::url_args(GH_URL))).expect("again");
        assert!(again.contains("already watching"));
        assert!(
            rig.pr(PrVerb::Watch(Rig::url_args("https://example.com/x")))
                .is_err()
        );
        let gone = rig
            .pr(PrVerb::Unwatch(Rig::url_args(GH_URL)))
            .expect("unwatch");
        assert!(gone.contains("stopped watching"));
        assert!(
            rig.pr(PrVerb::List(Rig::session_args()))
                .expect("list")
                .is_empty()
        );
        let no_session = run_with(
            &PrArgs {
                command: PrVerb::List(SessionArgs { session: None }),
            },
            &mut Vec::new(),
            &rig.repo,
            &rig.lookup(),
        );
        assert!(no_session.is_err(), "needs a session");
    }

    #[test]
    fn poll_mails_wakes_to_the_owning_session_once() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        rig.poll_mailed();
        let mails = rig.mails();
        assert_eq!(mails.len(), 1, "one mail for one poll");
        let (_, msg) = &mails[0];
        assert_eq!(msg.to_session.as_deref(), Some(OWNER));
        assert_eq!(msg.from_agent, "zirv-pr-watch");
        for needle in [
            "Checks failed on 0123456",
            "deploy (cancelled)",
            "untrusted reviewer text",
            "rename `foo`",
            "src/lib.rs",
            "conflicts",
        ] {
            assert!(msg.body.contains(needle), "missing {needle}: {}", msg.body);
        }
        let rec = store::load(&rig.state(), OWNER, GH_URL).expect("record");
        assert_eq!(rec.wake_seq, 1);
        assert_eq!(rec.viewer.as_deref(), Some("me"));
        assert!(rec.last_polled > 0 && rec.last_error.is_none());
        assert!(
            rig.poll().contains("no change"),
            "the same read is not news"
        );
        assert_eq!(rig.mails().len(), 1);
        assert_eq!(
            store::load(&rig.state(), OWNER, GH_URL)
                .expect("record")
                .wake_seq,
            1
        );
    }

    #[test]
    fn a_merged_pull_request_sends_one_final_mail_and_drops_the_watch() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view-merged-minimal.json");
        rig.watch(GH_URL);
        rig.poll_mailed();
        let mails = rig.mails();
        assert_eq!(mails.len(), 1);
        assert!(mails[0].1.body.contains("was merged"));
        assert!(store::load(&rig.state(), OWNER, GH_URL).is_none());
    }

    #[test]
    fn a_gitlab_merge_request_is_read_with_glab_and_mailed() {
        let rig = Rig::new();
        rig.shim(
            "glab",
            "\"mr view\") cat \"$F/glab-mr-view.json\" ;;\n\
             \"api user\") echo '{\"username\":\"josj\"}' ;;\n\
             api*) case \"$2\" in *discussions*) cat \"$F/glab-discussions.json\" ;; *jobs*) cat \"$F/glab-jobs.json\" ;; *) exit 9 ;; esac ;;\n\
             *) exit 9 ;;",
        );
        rig.watch(GL_URL);
        rig.poll_mailed();
        let body = &rig.mails()[0].1.body;
        assert!(body.contains("Checks failed"), "{body}");
        assert!(body.contains("Please add a test"), "{body}");
    }

    #[test]
    fn a_missing_cli_records_no_cli_and_sends_no_mail() {
        let rig = Rig::new();
        rig.watch(GH_URL);
        assert!(rig.poll().contains("error: no_cli"));
        assert!(rig.mails().is_empty());
        let rec = store::load(&rig.state(), OWNER, GH_URL).expect("record");
        assert_eq!(rec.last_error.map(|e| e.kind), Some(Failure::NoCli));
        assert_eq!(rec.consecutive_failures, 1);
    }

    #[test]
    fn a_double_not_found_stops_the_watch_without_mail() {
        let rig = Rig::new();
        rig.shim("gh", "*) cat \"$F/gh-404.json\" >&2; exit 1 ;;");
        rig.watch(GH_URL);
        assert!(rig.poll().contains("error: not_found"));
        assert!(rig.poll().contains("error: not_found"));
        assert!(rig.poll().contains("stopped (not_found)"));
        assert!(rig.mails().is_empty());
    }

    #[test]
    fn a_poller_that_cannot_take_the_lock_skips_the_watch() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        let held = store::try_lock(&rig.state(), OWNER, GH_URL).expect("first poller");
        assert!(rig.poll().contains("busy"));
        assert!(rig.mails().is_empty(), "the busy poller sent nothing");
        drop(held);
        rig.poll_mailed();
    }

    #[test]
    fn watch_refuses_hosts_that_are_not_git_remotes_ip_literals_and_ports() {
        let rig = Rig::new();
        for (url, needle) in [
            (
                "https://evil.example/acme/widgets/pull/1",
                "github.com, gitlab.example.com",
            ),
            ("https://127.0.0.1/acme/widgets/pull/1", "not a GitHub"),
            ("https://[::1]/acme/widgets/pull/1", "not a GitHub"),
            (
                "https://github.com:8443/acme/widgets/pull/1",
                "not a GitHub",
            ),
        ] {
            let err = rig
                .pr(PrVerb::Watch(Rig::url_args(url)))
                .expect_err(url)
                .to_string();
            assert!(err.contains(needle), "{url}: {err}");
        }
        assert!(
            rig.pr(PrVerb::List(Rig::session_args()))
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn watch_accepts_hosts_of_ssh_and_scp_style_remotes_case_insensitively() {
        let rig = Rig::new();
        for (name, url) in [
            ("ssh", "ssh://git@ghe.corp.example:2222/acme/widgets.git"),
            ("scp", "git@GL.corp.example:group/proj.git"),
        ] {
            assert!(worktree::tests::run(
                &rig.repo,
                &["remote", "add", name, url]
            ));
        }
        for url in [
            "https://ghe.corp.example/acme/widgets/pull/1",
            "https://gl.corp.example/group/proj/-/merge_requests/2",
            "https://GitHub.com/acme/widgets/pull/3",
        ] {
            rig.pr(PrVerb::Watch(Rig::url_args(url))).expect(url);
        }
    }

    #[test]
    fn poll_refuses_a_tampered_stored_url_without_spawning_a_cli() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        let evil = "https://evil.example/acme/widgets/pull/12";
        let mut rec = store::load(&rig.state(), OWNER, GH_URL).expect("record");
        rec.url = evil.into();
        store::remove(&rig.state(), OWNER, GH_URL);
        store::save(&rig.state(), &rec).expect("save");
        assert!(rig.poll().contains("error: untrusted_host"));
        let rec = store::load(&rig.state(), OWNER, evil).expect("record");
        assert_eq!(rec.last_error.map(|e| e.kind), Some(Failure::UntrustedHost));
        assert!(!rig.bin.join("gh.env").exists(), "gh must not be spawned");
    }

    #[test]
    fn github_calls_always_pin_gh_host_even_for_github_com() {
        let rig = Rig::new();
        rig.gh_shim("gh-pr-view.json");
        rig.watch(GH_URL);
        rig.poll_mailed();
        let log = std::fs::read_to_string(rig.bin.join("gh.env")).expect("gh was spawned");
        assert!(
            log.lines().all(|line| line == "GH_HOST=github.com"),
            "{log}"
        );
    }
}
