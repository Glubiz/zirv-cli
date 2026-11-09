//! Issue #946: per-turn git checkpoints. [`capture`] snapshots a working tree
//! (tracked, staged, unstaged and untracked-but-not-ignored files) into a
//! commit under `refs/zirv/checkpoints/<session-short>/<seq>` without ever
//! touching HEAD, the real index or a single worktree file: it copies the
//! index to a temp file inside the git dir, stages everything into that copy,
//! writes a tree from it and commits that tree with no parent (order comes
//! from `Zirv-Seq`, so dropping a ref makes its commit unreachable). Metadata lives in `Zirv-*` commit trailers, so there is no state
//! file. Every failure is a typed [`Skip`], never a panic: a checkpoint must
//! never be able to worsen the session it observes.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::CtxResult;
use super::config::{CheckpointConfig, CtxConfig};
use super::state::now_secs;
use super::{sessions, worktree};

const REF_PREFIX: &str = "refs/zirv/checkpoints/";
/// Nested repositories without commits that one capture will exclude before giving up.
const MAX_NESTED_EXCLUDES: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const READ_CAP: usize = 256 * 1024;
const CAPTURE_CAP: usize = 4 * 1024 * 1024;

/// Why a capture did nothing. The session is never affected by any of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    NotARepo(String),
    BadSession,
    MidOperation(&'static str),
    Timeout(String),
    Git { step: String, message: String },
    Io(String),
    TooManyNestedRepos(usize),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::NotARepo(m) => write!(f, "not a git work tree: {m}"),
            Skip::BadSession => f.write_str("session id has no usable characters"),
            Skip::MidOperation(op) => write!(f, "a {op} is in progress"),
            Skip::Timeout(step) => write!(f, "git {step} timed out and was killed"),
            Skip::Git { step, message } => write!(f, "git {step} failed: {message}"),
            Skip::Io(m) => write!(f, "{m}"),
            Skip::TooManyNestedRepos(n) => write!(
                f,
                "more than {n} nested repositories without commits; nothing captured"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Baseline,
    Turn,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Baseline => "baseline",
            Kind::Turn => "turn",
        }
    }
}

pub struct Request<'a> {
    /// Any path inside the work tree to snapshot.
    pub repo: &'a Path,
    /// The session id (full or short); the ref namespace uses its short form.
    pub session: &'a str,
    pub agent: &'a str,
    /// Agent turn this snapshot follows; defaults to the sequence number.
    pub turn: Option<u64>,
    pub cfg: &'a CheckpointConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub short: String,
    pub seq: u64,
    pub commit: String,
    pub kind: Kind,
    /// `assume-unchanged`/`skip-worktree` entries mean the tree may not match the disk.
    pub partial: bool,
    /// Nested repositories without commits that were left out of the snapshot.
    pub excluded: Vec<String>,
    /// Older checkpoints whose refs retention just dropped.
    pub pruned: usize,
}

enum RunErr {
    Spawn(String),
    Timeout,
    Failed(String),
}

impl RunErr {
    fn skip(self, step: &str) -> Skip {
        match self {
            RunErr::Spawn(m) => Skip::Io(format!("git {step}: {m}")),
            RunErr::Timeout => Skip::Timeout(step.to_string()),
            RunErr::Failed(message) => Skip::Git {
                step: step.to_string(),
                message,
            },
        }
    }
}

/// Keeps the first `cap` bytes and drains the rest, so a chatty git can never block on a full pipe.
fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let take = count.min(cap.saturating_sub(kept.len()));
        kept.extend_from_slice(chunk.get(..take).unwrap_or_default());
        truncated |= take < count;
    }
    (kept, truncated)
}

/// Runs `cmd` with a hard deadline: past it git is killed and reaped.
fn run_bounded(cmd: &mut Command, timeout: Duration, cap: usize) -> Result<(String, bool), RunErr> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunErr::Spawn(e.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .map(|s| std::thread::spawn(move || read_capped(s, cap)));
    let stderr = child
        .stderr
        .take()
        .map(|s| std::thread::spawn(move || read_capped(s, 16 * 1024)));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                let _ = child.kill();
                break child.wait().ok();
            }
        }
    };
    let (out, truncated) = stdout.and_then(|t| t.join().ok()).unwrap_or_default();
    let (err, _) = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    if timed_out {
        return Err(RunErr::Timeout);
    }
    match status {
        Some(status) if status.success() => {
            Ok((String::from_utf8_lossy(&out).into_owned(), truncated))
        }
        _ => Err(RunErr::Failed(
            String::from_utf8_lossy(&err).trim().to_string(),
        )),
    }
}

/// `git` scoped to `dir` with the settings every checkpoint command shares.
fn git(dir: &Path, index: Option<&Path>) -> Command {
    let mut cmd = worktree::git_command(dir);
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.splitIndex=false",
        "-c",
        "core.safecrlf=false",
        "-c",
        "commit.gpgsign=false",
    ])
    .env("GIT_OPTIONAL_LOCKS", "0")
    .env("GIT_AUTHOR_NAME", "zirv")
    .env("GIT_AUTHOR_EMAIL", "zirv@localhost")
    .env("GIT_COMMITTER_NAME", "zirv")
    .env("GIT_COMMITTER_EMAIL", "zirv@localhost");
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd
}

/// The temp index; its file and lock are removed on every exit path.
struct TempIndex(PathBuf);

impl TempIndex {
    fn lock_path(&self) -> PathBuf {
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        PathBuf::from(lock)
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.lock_path());
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Trailer values come from the environment; keep them on one line.
fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `error: 'dir/' does not have a commit checked out` offenders in git add's stderr.
fn nested_offenders(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("error: '")?;
            let path = rest.strip_suffix("' does not have a commit checked out")?;
            Some(path.trim_end_matches('/').to_string())
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    short: String,
    seq: u64,
    commit: String,
    ts: u64,
}

/// Checkpoint refs for `short`, or for every session when `None`; unordered.
fn entries(repo: &Path, short: Option<&str>) -> Result<Vec<Entry>, RunErr> {
    let prefix = match short {
        Some(short) => format!("{REF_PREFIX}{short}/"),
        None => REF_PREFIX.to_string(),
    };
    let (out, _) = run_bounded(
        git(repo, None).args([
            "for-each-ref",
            "--format=%(refname)%09%(objectname)%09%(committerdate:unix)",
            &prefix,
        ]),
        READ_TIMEOUT,
        READ_CAP * 16,
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.strip_prefix(REF_PREFIX)?;
            let (short, seq) = name.split_once('/')?;
            Some(Entry {
                short: short.to_string(),
                seq: seq.parse().ok()?,
                commit: parts.next()?.to_string(),
                ts: parts.next()?.parse().ok()?,
            })
        })
        .collect())
}

/// Pure: sequence numbers retention drops -- everything past the newest `max`
/// and everything older than `ttl_secs`. The newest entry always survives.
fn select_prune(entries: &[Entry], max: usize, ttl_secs: u64, now: u64) -> Vec<u64> {
    let mut seqs: Vec<(u64, u64)> = entries.iter().map(|e| (e.seq, e.ts)).collect();
    seqs.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    seqs.iter()
        .enumerate()
        .skip(1)
        .filter(|(rank, (_, ts))| *rank >= max || now.saturating_sub(*ts) > ttl_secs)
        .map(|(_, (seq, _))| *seq)
        .collect()
}

/// Snapshots the work tree around `req.repo`; see the module docs for the mechanism.
pub fn capture(req: &Request<'_>) -> Result<Captured, Skip> {
    let short = sessions::short_id(req.session);
    if short.is_empty() {
        return Err(Skip::BadSession);
    }
    let deadline = Instant::now() + Duration::from_secs(req.cfg.capture_timeout_secs);
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let step = |cmd: &mut Command, name: &str| -> Result<String, Skip> {
        run_bounded(cmd, remaining(), CAPTURE_CAP)
            .map(|(out, _)| out)
            .map_err(|e| e.skip(name))
    };

    let probe = run_bounded(
        git(req.repo, None).args(["rev-parse", "--absolute-git-dir", "--show-toplevel"]),
        remaining(),
        READ_CAP,
    )
    .map_err(|e| match e {
        RunErr::Failed(m) => Skip::NotARepo(m),
        other => other.skip("rev-parse"),
    })?
    .0;
    let mut lines = probe.lines();
    let (Some(git_dir), Some(top)) = (lines.next(), lines.next()) else {
        return Err(Skip::NotARepo("rev-parse printed no paths".to_string()));
    };
    let (git_dir, top) = (PathBuf::from(git_dir), PathBuf::from(top));

    for (op, marker) in [
        ("merge", "MERGE_HEAD"),
        ("rebase", "rebase-merge"),
        ("rebase", "rebase-apply"),
    ] {
        if git_dir.join(marker).exists() {
            return Err(Skip::MidOperation(op));
        }
    }

    let head = match run_bounded(
        git(&top, None).args(["rev-parse", "--verify", "-q", "HEAD"]),
        remaining(),
        READ_CAP,
    ) {
        Ok((out, _)) => Some(out.trim().to_string()),
        Err(RunErr::Failed(_)) => None,
        Err(other) => return Err(other.skip("rev-parse HEAD")),
    };

    let existing = entries(&top, Some(&short)).map_err(|e| e.skip("for-each-ref"))?;
    let previous = existing.iter().max_by_key(|e| e.seq);
    let seq = previous.map_or(0, |e| e.seq.saturating_add(1));
    let kind = if previous.is_none() {
        Kind::Baseline
    } else {
        Kind::Turn
    };

    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = TempIndex(git_dir.join(format!("zirv-ckpt-index-{}-{n}", std::process::id())));
    let real_index = git_dir.join("index");
    if real_index.exists() {
        std::fs::copy(&real_index, &temp.0).map_err(|e| Skip::Io(format!("copy index: {e}")))?;
    } else if head.is_some() {
        step(
            git(&top, Some(&temp.0)).args(["read-tree", "HEAD"]),
            "read-tree",
        )?;
    }

    let flags = step(
        git(&top, Some(&temp.0)).args(["ls-files", "-v"]),
        "ls-files",
    )?;
    let partial = flags
        .lines()
        .filter_map(|line| line.chars().next())
        .any(|tag| tag.is_ascii_lowercase() || tag == 'S');

    let mut excluded: Vec<String> = Vec::new();
    loop {
        let mut add = git(&top, Some(&temp.0));
        add.args(["add", "-A", "--", "."]);
        for dir in &excluded {
            add.arg(format!(":(exclude,literal){dir}"));
        }
        match run_bounded(&mut add, remaining(), CAPTURE_CAP) {
            Ok(_) => break,
            Err(RunErr::Failed(message)) => {
                let fresh: Vec<String> = nested_offenders(&message)
                    .into_iter()
                    .filter(|dir| !excluded.contains(dir))
                    .collect();
                if fresh.is_empty() {
                    return Err(Skip::Git {
                        step: "add".to_string(),
                        message,
                    });
                }
                if excluded.len() + fresh.len() > MAX_NESTED_EXCLUDES {
                    return Err(Skip::TooManyNestedRepos(MAX_NESTED_EXCLUDES));
                }
                excluded.extend(fresh);
                let _ = std::fs::remove_file(temp.lock_path());
            }
            Err(other) => return Err(other.skip("add")),
        }
    }

    let tree = step(git(&top, Some(&temp.0)).arg("write-tree"), "write-tree")?;
    let tree = tree.trim();
    let trailers = [
        ("Zirv-Session", req.session.to_string()),
        ("Zirv-Seq", seq.to_string()),
        ("Zirv-Turn", req.turn.unwrap_or(seq).to_string()),
        ("Zirv-Agent", req.agent.to_string()),
        (
            "Zirv-Head",
            head.clone().unwrap_or_else(|| "unborn".to_string()),
        ),
        ("Zirv-Kind", kind.as_str().to_string()),
        ("Zirv-Partial", partial.to_string()),
    ]
    .iter()
    .map(|(key, value)| format!("{key}: {}", one_line(value)))
    .collect::<Vec<_>>()
    .join("\n");
    let mut commit_tree = git(&top, None);
    commit_tree.args(["commit-tree", tree]);
    commit_tree.args([
        "-m",
        &format!("zirv checkpoint {short}/{seq}"),
        "-m",
        &trailers,
    ]);
    let commit = step(&mut commit_tree, "commit-tree")?.trim().to_string();

    let refname = format!("{REF_PREFIX}{short}/{seq}");
    step(
        git(&top, None).args(["update-ref", &refname, &commit]),
        "update-ref",
    )?;

    let mut all = existing;
    all.push(Entry {
        short: short.clone(),
        seq,
        commit: commit.clone(),
        ts: now_secs(),
    });
    let max = usize::try_from(req.cfg.max_per_session).unwrap_or(usize::MAX);
    let mut pruned = 0;
    for dropped in select_prune(&all, max, req.cfg.ttl_secs, now_secs()) {
        let dropped_ref = format!("{REF_PREFIX}{short}/{dropped}");
        if step(
            git(&top, None).args(["update-ref", "-d", &dropped_ref]),
            "update-ref -d",
        )
        .is_ok()
        {
            pruned += 1;
        }
    }

    Ok(Captured {
        short,
        seq,
        commit,
        kind,
        partial,
        excluded,
        pruned,
    })
}

/// One checkpoint read back from its commit and trailers.
#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub session: String,
    pub seq: u64,
    pub commit: String,
    pub ts: u64,
    pub kind: String,
    pub turn: String,
    pub head: String,
    pub partial: bool,
    pub files: usize,
    pub added: u64,
    pub deleted: u64,
}

/// What checkpoint `seq` is compared against: the previous checkpoint of its
/// session, else the HEAD it was taken on (checkpoint commits have no parents).
fn base_of(found: &[Entry], short: &str, seq: u64, head: &str) -> Option<String> {
    let previous = found
        .iter()
        .filter(|e| e.short == short && e.seq < seq)
        .max_by_key(|e| e.seq);
    match previous {
        Some(previous) => Some(previous.commit.clone()),
        None if head != "unborn" && !head.is_empty() => Some(head.to_string()),
        None => None,
    }
}

fn trailer(message: &str, key: &str) -> String {
    let prefix = format!("{key}: ");
    message
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or("-")
        .to_string()
}

/// Checkpoints of `short` (or of every session), newest first.
pub fn list(repo: &Path, short: Option<&str>) -> Result<Vec<Info>, String> {
    let mut found = entries(repo, short).map_err(|e| e.skip("for-each-ref").to_string())?;
    found.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.seq.cmp(&a.seq)));
    let mut infos = Vec::new();
    for entry in found.clone() {
        let (message, _) = run_bounded(
            git(repo, None).args(["log", "-1", "--format=%B", &entry.commit]),
            READ_TIMEOUT,
            READ_CAP,
        )
        .map_err(|e| e.skip("log").to_string())?;
        let head = trailer(&message, "Zirv-Head");
        let base = base_of(&found, &entry.short, entry.seq, &head);
        let mut numstat = git(repo, None);
        numstat.args(["diff-tree", "-r", "--root", "--no-commit-id", "--numstat"]);
        if let Some(base) = &base {
            numstat.arg(base);
        }
        numstat.arg(&entry.commit);
        let (stat, _) = run_bounded(&mut numstat, READ_TIMEOUT, READ_CAP * 16)
            .map_err(|e| e.skip("diff-tree").to_string())?;
        let (mut files, mut added, mut deleted) = (0, 0u64, 0u64);
        for line in stat.lines() {
            let mut cols = line.split('\t');
            files += 1;
            added += cols.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
            deleted += cols.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        }
        infos.push(Info {
            session: entry.short,
            seq: entry.seq,
            commit: entry.commit,
            ts: entry.ts,
            kind: trailer(&message, "Zirv-Kind"),
            turn: trailer(&message, "Zirv-Turn"),
            head,
            partial: trailer(&message, "Zirv-Partial") == "true",
            files,
            added,
            deleted,
        });
    }
    Ok(infos)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Patch,
    Stat,
    NameStatus,
}

/// `spec` is `<seq>` (that checkpoint against its base) or `<a>..<b>`; the
/// output is capped, and the flag reports a cut.
pub fn diff(
    repo: &Path,
    short: &str,
    spec: &str,
    mode: DiffMode,
) -> Result<(String, bool), String> {
    let found = entries(repo, Some(short)).map_err(|e| e.skip("for-each-ref").to_string())?;
    let commit_of = |raw: &str| -> Result<String, String> {
        let seq: u64 = raw
            .trim()
            .parse()
            .map_err(|_| format!("'{raw}' is not a checkpoint sequence number"))?;
        found
            .iter()
            .find(|e| e.seq == seq)
            .map(|e| e.commit.clone())
            .ok_or_else(|| format!("no checkpoint {seq} for session {short}"))
    };
    let mut cmd = git(repo, None);
    cmd.args([
        "diff-tree",
        "-r",
        "--root",
        "--no-commit-id",
        "--no-ext-diff",
        "--no-color",
    ]);
    cmd.arg(match mode {
        DiffMode::Patch => "-p",
        DiffMode::Stat => "--stat",
        DiffMode::NameStatus => "--name-status",
    });
    match spec.split_once("..") {
        Some((a, b)) => {
            cmd.arg(commit_of(a)?).arg(commit_of(b)?);
        }
        None => {
            let commit = commit_of(spec)?;
            let (message, _) = run_bounded(
                git(repo, None).args(["log", "-1", "--format=%B", &commit]),
                READ_TIMEOUT,
                READ_CAP,
            )
            .map_err(|e| e.skip("log").to_string())?;
            let head = trailer(&message, "Zirv-Head");
            let seq: u64 = spec.trim().parse().unwrap_or_default();
            if let Some(base) = base_of(&found, short, seq, &head) {
                cmd.arg(base);
            }
            cmd.arg(commit);
        }
    }
    run_bounded(&mut cmd, READ_TIMEOUT, READ_CAP).map_err(|e| e.skip("diff-tree").to_string())
}

#[derive(Debug, clap::Args)]
pub struct CheckpointArgs {
    #[command(subcommand)]
    pub command: CheckpointVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum CheckpointVerb {
    /// Snapshot the current work tree into `refs/zirv/checkpoints/` for this
    /// session (or `--session`). Refused unless `[checkpoint] enabled` is set
    /// by the operator. Never touches HEAD, the index or any file.
    Capture(CaptureArgs),
    /// List checkpoints: SEQ TURN TIME KIND FILES +/- HEAD, newest first.
    List(ListArgs),
    /// Show what a checkpoint changed: `<seq>` against its base, or `<a>..<b>`.
    Diff(DiffArgs),
}

#[derive(Debug, clap::Args)]
pub struct CaptureArgs {
    /// Session id or short id; defaults to the current zirv session.
    #[arg(long)]
    pub session: Option<String>,
    /// Agent turn number recorded in the `Zirv-Turn` trailer.
    #[arg(long)]
    pub turn: Option<u64>,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Session id or short id; defaults to the current session, else every session.
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct DiffArgs {
    /// `<seq>` or `<a>..<b>`.
    pub spec: String,
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long, conflicts_with = "name_status")]
    pub stat: bool,
    #[arg(long)]
    pub name_status: bool,
}

fn format_time(ts: u64) -> String {
    i64::try_from(ts)
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map_or_else(
            || ts.to_string(),
            |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
        )
}

pub fn run_capture<W: Write>(
    cfg: &CheckpointConfig,
    repo: &Path,
    w: &mut W,
    args: &CaptureArgs,
    session: Option<String>,
    agent: &str,
) -> CtxResult<i32> {
    if !cfg.enabled {
        writeln!(
            w,
            "checkpoints are disabled: set `[checkpoint] enabled = true` in ~/.zirv/ctx.toml (or ZIRV_CTX_CHECKPOINT=1); a repository cannot enable them"
        )?;
        return Ok(1);
    }
    let Some(session) = args.session.clone().or(session) else {
        writeln!(
            w,
            "no session: pass --session <id> or run inside a zirv session"
        )?;
        return Ok(1);
    };
    let request = Request {
        repo,
        session: &session,
        agent,
        turn: args.turn,
        cfg,
    };
    match capture(&request) {
        Ok(done) => {
            writeln!(
                w,
                "checkpoint {}/{} {} kind={} partial={}",
                done.short,
                done.seq,
                done.commit.get(..12).unwrap_or(&done.commit),
                done.kind.as_str(),
                done.partial
            )?;
            for dir in &done.excluded {
                writeln!(w, "excluded nested repository without commits: {dir}")?;
            }
            if done.pruned > 0 {
                writeln!(w, "retention dropped {} older checkpoint(s)", done.pruned)?;
            }
            Ok(0)
        }
        Err(skip) => {
            writeln!(w, "checkpoint skipped: {skip}")?;
            Ok(1)
        }
    }
}

pub fn run_list<W: Write>(
    repo: &Path,
    w: &mut W,
    args: &ListArgs,
    session: Option<String>,
) -> CtxResult<i32> {
    let short = args
        .session
        .clone()
        .or(session)
        .map(|s| sessions::short_id(&s));
    let infos = match list(repo, short.as_deref()) {
        Ok(infos) => infos,
        Err(message) => {
            writeln!(w, "checkpoint list failed: {message}")?;
            return Ok(1);
        }
    };
    if args.json {
        writeln!(w, "{}", serde_json::to_string(&infos)?)?;
        return Ok(0);
    }
    if infos.is_empty() {
        writeln!(w, "no checkpoints")?;
        return Ok(0);
    }
    writeln!(w, "SESSION SEQ TURN TIME KIND FILES +/- HEAD")?;
    for i in &infos {
        writeln!(
            w,
            "{} {} {} {} {}{} {} +{}/-{} {}",
            i.session,
            i.seq,
            i.turn,
            format_time(i.ts),
            i.kind,
            if i.partial { " (partial)" } else { "" },
            i.files,
            i.added,
            i.deleted,
            i.head.get(..12).unwrap_or(&i.head),
        )?;
    }
    Ok(0)
}

pub fn run_diff<W: Write>(
    repo: &Path,
    w: &mut W,
    args: &DiffArgs,
    session: Option<String>,
) -> CtxResult<i32> {
    let Some(session) = args.session.clone().or(session) else {
        writeln!(
            w,
            "no session: pass --session <id> or run inside a zirv session"
        )?;
        return Ok(1);
    };
    let mode = if args.stat {
        DiffMode::Stat
    } else if args.name_status {
        DiffMode::NameStatus
    } else {
        DiffMode::Patch
    };
    match diff(repo, &sessions::short_id(&session), &args.spec, mode) {
        Ok((text, truncated)) => {
            write!(w, "{text}")?;
            if truncated {
                writeln!(
                    w,
                    "[diff truncated at {READ_CAP} bytes; use --stat or --name-status]"
                )?;
            }
            Ok(0)
        }
        Err(message) => {
            writeln!(w, "checkpoint diff failed: {message}")?;
            Ok(1)
        }
    }
}

pub fn run<W: Write>(args: &CheckpointArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let repo = std::env::current_dir()?;
    let session = std::env::var(super::adapters::SESSION_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty());
    match &args.command {
        CheckpointVerb::Capture(a) => {
            let cfg = CtxConfig::load(&repo, &env)?.checkpoint;
            let agent =
                std::env::var(super::adapters::AGENT_ENV).unwrap_or_else(|_| "manual".to_string());
            run_capture(&cfg, &repo, w, a, session, &agent)
        }
        CheckpointVerb::List(a) => run_list(&repo, w, a, session),
        CheckpointVerb::Diff(a) => run_diff(&repo, w, a, session),
    }
}

#[cfg(test)]
mod tests {
    use super::worktree::tests::{commit, git_available, init_repo, run as git_ok};
    use super::*;

    fn cfg() -> CheckpointConfig {
        CheckpointConfig {
            enabled: true,
            ..CheckpointConfig::default()
        }
    }

    fn snap(dir: &Path, session: &str, cfg: &CheckpointConfig) -> Result<Captured, Skip> {
        capture(&Request {
            repo: dir,
            session,
            agent: "claude",
            turn: None,
            cfg,
        })
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn show(dir: &Path, commit: &str, path: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["show", &format!("{commit}:{path}")])
            .output()
            .expect("git");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        commit(dir.path(), "a.txt", "one\n", "init");
        dir
    }

    fn index_stamp(dir: &Path) -> (Vec<u8>, std::time::SystemTime) {
        let path = dir.join(".git/index");
        (
            std::fs::read(&path).expect("index"),
            std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .expect("mtime"),
        )
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir.join(".git"))
            .expect("read .git")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("zirv-ckpt-index"))
            .collect()
    }

    #[test]
    fn capture_leaves_head_index_status_and_files_untouched() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        std::fs::write(p.join("a.txt"), "two\n").expect("write");
        std::fs::write(p.join("new.txt"), "n\n").expect("write");
        assert!(git_ok(p, &["add", "a.txt"]));
        std::fs::write(p.join("a.txt"), "three\n").expect("write");
        let head = git_out(p, &["rev-parse", "HEAD"]);
        // `git status` refreshes the index itself, so take it before the stamp.
        let status = git_out(p, &["status", "--porcelain"]);
        let (index, mtime) = index_stamp(p);
        let a = std::fs::read(p.join("a.txt")).expect("read");

        snap(p, "sess1234abcd", &cfg()).expect("capture");

        assert_eq!(git_out(p, &["rev-parse", "HEAD"]), head);
        assert_eq!(index_stamp(p), (index, mtime));
        assert_eq!(git_out(p, &["status", "--porcelain"]), status);
        assert_eq!(std::fs::read(p.join("a.txt")).expect("read"), a);
        assert!(leftovers(p).is_empty());
    }

    #[test]
    fn staged_unstaged_and_untracked_are_captured_and_ignored_is_not() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        std::fs::write(p.join(".gitignore"), "secret.log\n").expect("write");
        assert!(git_ok(p, &["add", ".gitignore"]));
        assert!(git_ok(p, &["commit", "-q", "-m", "ignore"]));
        std::fs::write(p.join("staged.txt"), "staged\n").expect("write");
        assert!(git_ok(p, &["add", "staged.txt"]));
        std::fs::write(p.join("a.txt"), "unstaged\n").expect("write");
        std::fs::write(p.join("untracked.txt"), "untracked\n").expect("write");
        std::fs::write(p.join("secret.log"), "nope\n").expect("write");

        let done = snap(p, "sess1234abcd", &cfg()).expect("capture");
        let c = &done.commit;
        assert_eq!(show(p, c, "staged.txt").as_deref(), Some("staged\n"));
        assert_eq!(show(p, c, "a.txt").as_deref(), Some("unstaged\n"));
        assert_eq!(show(p, c, "untracked.txt").as_deref(), Some("untracked\n"));
        assert_eq!(show(p, c, "secret.log"), None);
    }

    #[test]
    fn a_deletion_is_recorded() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        std::fs::remove_file(p.join("a.txt")).expect("rm");
        let done = snap(p, "sess1234abcd", &cfg()).expect("capture");
        assert_eq!(show(p, &done.commit, "a.txt"), None);
    }

    #[test]
    fn checkpoints_are_parentless_ordered_by_seq_and_trailers_round_trip() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        let first = snap(p, "sess1234abcd", &cfg()).expect("first");
        std::fs::write(p.join("b.txt"), "b\n").expect("write");
        let second = capture(&Request {
            repo: p,
            session: "sess1234abcd",
            agent: "claude",
            turn: Some(7),
            cfg: &cfg(),
        })
        .expect("second");
        assert_eq!((first.seq, second.seq), (0, 1));
        assert_eq!((first.kind, second.kind), (Kind::Baseline, Kind::Turn));
        assert_eq!(
            git_out(p, &["log", "-1", "--format=%P", &second.commit]).trim(),
            "",
            "a checkpoint must be parentless so pruning its ref frees the commit"
        );
        assert_eq!(
            git_out(p, &["rev-parse", "refs/zirv/checkpoints/sess1234/1"]).trim(),
            second.commit
        );
        let message = git_out(p, &["log", "-1", "--format=%B", &second.commit]);
        let head = git_out(p, &["rev-parse", "HEAD"]);
        assert_eq!(trailer(&message, "Zirv-Session"), "sess1234abcd");
        assert_eq!(trailer(&message, "Zirv-Seq"), "1");
        assert_eq!(trailer(&message, "Zirv-Turn"), "7");
        assert_eq!(trailer(&message, "Zirv-Agent"), "claude");
        assert_eq!(trailer(&message, "Zirv-Head"), head.trim());
        assert_eq!(trailer(&message, "Zirv-Kind"), "turn");
        assert_eq!(trailer(&message, "Zirv-Partial"), "false");
    }

    #[test]
    fn a_nested_repo_without_commits_is_excluded_and_capture_succeeds() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        let inner = p.join("inner");
        std::fs::create_dir(&inner).expect("mkdir");
        init_repo(&inner);
        std::fs::write(inner.join("x.txt"), "x\n").expect("write");
        std::fs::write(p.join("outer.txt"), "o\n").expect("write");

        let done = snap(p, "sess1234abcd", &cfg()).expect("capture");
        assert_eq!(done.excluded, vec!["inner".to_string()]);
        assert_eq!(show(p, &done.commit, "outer.txt").as_deref(), Some("o\n"));
        assert_eq!(show(p, &done.commit, "inner/x.txt"), None);
        assert!(leftovers(p).is_empty());
    }

    #[test]
    fn a_merge_in_progress_is_skipped() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        std::fs::write(
            p.join(".git/MERGE_HEAD"),
            "0000000000000000000000000000000000000000\n",
        )
        .expect("write");
        assert_eq!(
            snap(p, "sess1234abcd", &cfg()),
            Err(Skip::MidOperation("merge"))
        );
        assert!(git_out(p, &["for-each-ref", "refs/zirv"]).is_empty());
    }

    #[test]
    fn the_temp_index_is_removed_when_capture_fails() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        // An unreadable path makes `git add` fail after the temp index exists.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(p.join("locked.txt"), "x\n").expect("write");
            std::fs::set_permissions(p.join("locked.txt"), std::fs::Permissions::from_mode(0o000))
                .expect("chmod");
            let unreadable = std::fs::read(p.join("locked.txt")).is_err();
            let result = snap(p, "sess1234abcd", &cfg());
            if unreadable {
                assert!(matches!(result, Err(Skip::Git { .. })), "{result:?}");
            }
            std::fs::set_permissions(p.join("locked.txt"), std::fs::Permissions::from_mode(0o644))
                .expect("chmod");
        }
        assert!(leftovers(p).is_empty());
        assert!(git_out(p, &["for-each-ref", "refs/zirv"]).is_empty() || cfg!(not(unix)));
    }

    #[test]
    fn an_unborn_head_and_a_non_repo_are_typed_results() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            snap(dir.path(), "sess1234abcd", &cfg()),
            Err(Skip::NotARepo(_))
        ));
        init_repo(dir.path());
        std::fs::write(dir.path().join("f.txt"), "f\n").expect("write");
        let done = snap(dir.path(), "sess1234abcd", &cfg()).expect("unborn capture");
        assert_eq!(
            show(dir.path(), &done.commit, "f.txt").as_deref(),
            Some("f\n")
        );
        assert_eq!(snap(dir.path(), "---", &cfg()), Err(Skip::BadSession));
    }

    #[test]
    fn retention_keeps_the_newest_max_per_session() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        let small = CheckpointConfig {
            max_per_session: 2,
            ..cfg()
        };
        for _ in 0..4 {
            snap(p, "sess1234abcd", &small).expect("capture");
        }
        let refs = git_out(p, &["for-each-ref", "--format=%(refname)", "refs/zirv"]);
        assert_eq!(
            refs.lines().collect::<Vec<_>>(),
            vec![
                "refs/zirv/checkpoints/sess1234/2",
                "refs/zirv/checkpoints/sess1234/3"
            ]
        );
    }

    #[test]
    fn a_pruned_checkpoint_is_no_longer_reachable_from_any_zirv_ref() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        let small = CheckpointConfig {
            max_per_session: 1,
            ..cfg()
        };
        let first = snap(p, "sess1234abcd", &small).expect("first");
        std::fs::write(p.join("b.txt"), "b\n").expect("write");
        let second = snap(p, "sess1234abcd", &small).expect("second");
        assert_eq!(second.pruned, 1);
        let reachable = git_out(p, &["rev-list", "--glob=refs/zirv/*"]);
        assert!(!reachable.contains(&first.commit), "{reachable}");
        assert!(reachable.contains(&second.commit));
    }

    #[test]
    fn select_prune_applies_count_and_ttl_but_never_drops_the_newest() {
        let e = |seq, ts| Entry {
            short: "s".into(),
            seq,
            commit: String::new(),
            ts,
        };
        let all = [e(0, 100), e(1, 900), e(2, 950), e(3, 990)];
        assert_eq!(select_prune(&all, 50, 1000, 1000), Vec::<u64>::new());
        assert_eq!(select_prune(&all, 2, 1000, 1000), vec![1, 0]);
        assert_eq!(select_prune(&all, 50, 60, 1000), vec![1, 0]);
        assert_eq!(select_prune(&all, 50, 0, 1000), vec![2, 1, 0]);
    }

    #[test]
    fn list_and_diff_read_checkpoints_back() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let p = dir.path();
        snap(p, "sess1234abcd", &cfg()).expect("first");
        std::fs::write(p.join("b.txt"), "b\n").expect("write");
        snap(p, "sess1234abcd", &cfg()).expect("second");

        let infos = list(p, Some("sess1234")).expect("list");
        assert_eq!(infos.iter().map(|i| i.seq).collect::<Vec<_>>(), vec![1, 0]);
        assert_eq!((infos[0].files, infos[0].added), (1, 1));
        assert_eq!(infos[1].kind, "baseline");

        let (names, truncated) = diff(p, "sess1234", "1", DiffMode::NameStatus).expect("diff");
        assert!(!truncated);
        assert_eq!(names.trim(), "A\tb.txt");
        let (range, _) = diff(p, "sess1234", "0..1", DiffMode::NameStatus).expect("range");
        assert_eq!(range.trim(), "A\tb.txt");
        assert!(diff(p, "sess1234", "9", DiffMode::Stat).is_err());
    }

    #[test]
    fn capture_is_refused_when_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        let code = run_capture(
            &CheckpointConfig::default(),
            dir.path(),
            &mut out,
            &CaptureArgs {
                session: Some("sess1234".into()),
                turn: None,
            },
            None,
            "manual",
        )
        .expect("run");
        assert_eq!(code, 1);
        assert!(String::from_utf8_lossy(&out).contains("disabled"));
        assert!(!dir.path().join(".git").exists());
    }

    #[test]
    fn a_repo_layer_may_only_narrow_checkpoint_config() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        let empty = |_: &str| None::<String>;

        let widen = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(widen.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            widen.path().join(".zirv/ctx.toml"),
            "[checkpoint]\nenabled = true\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(widen.path(), &empty)
            .expect("a repo enabled=true loads")
            .checkpoint;
        assert!(!cfg.enabled, "a repo may not turn checkpoints on");

        let off = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(off.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            off.path().join(".zirv/ctx.toml"),
            "[checkpoint]\nenabled = false\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(off.path(), &empty)
            .expect("a repo enabled=false loads")
            .checkpoint;
        assert!(!cfg.enabled);
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[checkpoint]\nenabled = true\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(off.path(), &empty)
            .expect("load")
            .checkpoint;
        assert!(
            !cfg.enabled,
            "a repo may turn an operator-enabled setting off"
        );
        let cfg = CtxConfig::load(widen.path(), &empty)
            .expect("load")
            .checkpoint;
        assert!(cfg.enabled, "a repo true leaves the operator's setting on");
        std::fs::remove_file(home_dir.path().join(".zirv/ctx.toml")).expect("rm");

        let narrow = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(narrow.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            narrow.path().join(".zirv/ctx.toml"),
            "[checkpoint]\nmax_per_session = 5\nttl_secs = 99999999\ncapture_timeout_secs = 5\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(narrow.path(), &empty)
            .expect("load")
            .checkpoint;
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_per_session, 5, "a repo may lower the cap");
        assert_eq!(cfg.ttl_secs, 1_209_600, "a repo may not raise the TTL");
        assert_eq!(cfg.capture_timeout_secs, 5);

        let env = |k: &str| (k == "ZIRV_CTX_CHECKPOINT").then(|| "1".to_string());
        let cfg = CtxConfig::load(narrow.path(), &env)
            .expect("load")
            .checkpoint;
        assert!(cfg.enabled, "the operator environment may enable");
    }
}
