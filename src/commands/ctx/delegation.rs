//! The runtime-neutral delegation service (issue #479, roadmap N10).
//!
//! One orchestrator may be a legacy harness session or a native one; one
//! worker may be a legacy harness process or a native in-process session.
//! All four combinations have to share the SAME ownership, receipt, result
//! and continuation contracts, or an orchestrator cannot reason about its
//! own fleet. This module is those contracts, and it is deliberately the
//! only implementation of them: the CLI verbs (`zirv agent`, `zirv ctx
//! send|wait|inbox`) and the native delegation tools (`runtime::tools::
//! delegation`) both call the functions here rather than each growing their
//! own copy.
//!
//! # What is durable, and in which order
//!
//! [`record_launch`] writes a delegation record BEFORE any worker starts.
//! [`publish_terminal`] writes the terminal outcome BEFORE it mails
//! anything. That order is what makes a crash recoverable in the only
//! direction that is safe: a crash after persistence and before delivery
//! leaves an undelivered outcome a later sweep republishes, never a delivered
//! outcome with no durable record behind it.
//!
//! # Delivery identity, not exactly-once transport
//!
//! Mail is at-least-once. Every terminal publication carries a
//! [`delivery_identity`] -- `<delegation>:<attempt>:<revision>` -- and a
//! consumer calls [`consume_delivery`] with it. The first call returns
//! `true`, every later call with the same identity returns `false`, so a
//! duplicated transport delivery is idempotent at the consumer and a genuine
//! second outcome (a later attempt, or a later revision of the same attempt)
//! is still distinguishable.
//!
//! # Ownership
//!
//! Nothing here invents a second ownership store. Exclusive task ownership is
//! `task::claim_locked`'s card claim; exclusive checkout ownership is
//! `permit::acquire_writer`'s per-tree claim; provider token ownership is
//! `reservation`'s ledger. A native worker takes the SAME three, which is
//! what stops a native and a legacy worker from both holding one tree.
//! [`close`] releases them again while leaving the receipts alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::config::CtxConfig;
use super::runtime::RuntimeKind;
use super::state::{StateDir, create_private_dir_all, repo_slug, write_private};

/// Bumped whenever [`Record`]'s own shape changes. A consumer branches on
/// this, never on field presence.
pub const SCHEMA_VERSION: u32 = 1;

/// How far one delegation has got. `Launched` and `Running` both mean "no
/// terminal outcome yet"; the rest are terminal for the CURRENT attempt --
/// a follow-up appends a new attempt rather than reopening the old one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Launched,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Ownership released and the record retired. Receipts are preserved.
    Closed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Launched => "launched",
            Phase::Running => "running",
            Phase::Completed => "completed",
            Phase::Failed => "failed",
            Phase::Cancelled => "cancelled",
            Phase::Closed => "closed",
        }
    }

    /// Whether this attempt has stopped. `Closed` counts: a closed record has
    /// no live worker behind it either.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Phase::Completed | Phase::Failed | Phase::Cancelled | Phase::Closed
        )
    }
}

/// The stable worker handle. Deliberately independent of any provider
/// conversation id (an Anthropic/OpenAI response id, a harness rollout uuid):
/// those change on every resume and are not addressable by a parent, while
/// `delegation` is minted once by zirv and survives every attempt, restart
/// and rollover the work goes through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHandle {
    pub delegation: String,
    pub attempt: u32,
    pub runtime: RuntimeKind,
    /// The zirv session uuid of the worker this attempt launched.
    pub worker_session: String,
    pub short: String,
    pub role: String,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    pub workdir: PathBuf,
    /// Issue #541 chunk C, decision 2: the manifest identity this delegation
    /// was checked against (the caller's explicit choice, or the role's own
    /// default) -- `None` for a role outside the closed team, which skips
    /// the manifest/team-plan checks entirely. Older durable records default
    /// safely to `None`.
    #[serde(default)]
    pub manifest: Option<String>,
    /// Whether this delegation used the coordinator's `plan_override`
    /// escape from the "must match an unfilled team-plan seat" rule.
    #[serde(default)]
    pub plan_override: bool,
}

/// One attempt at the same delegation. A follow-up that has to launch a
/// continuation appends one of these; the prior attempt's outcome, result
/// path and evidence are never overwritten (issue #454: "do not reopen a
/// completed task by overwriting its prior verified outcome").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub attempt: u32,
    pub runtime: RuntimeKind,
    pub worker_session: String,
    pub short: String,
    pub started_at: u64,
    #[serde(default)]
    pub ended_at: Option<u64>,
    pub phase: Phase,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub result_path: Option<PathBuf>,
    #[serde(default)]
    pub summary: Option<String>,
}

/// A message that could not be delivered when it was submitted, kept durably
/// so the retry at the next boundary is a fact rather than an in-memory hope
/// (issue #468: an advisory dropped because a permission dialog was open was
/// never retried).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: String,
    pub body: String,
    pub queued_at: u64,
    /// `attention::block_reason`'s own label for why the boundary was wrong.
    pub reason: String,
}

/// The durable record. One JSON file per delegation under
/// `<state>/delegations/<repo-slug>/<delegation>.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema_version: u32,
    pub handle: WorkerHandle,
    #[serde(default)]
    pub parent_session: Option<String>,
    pub phase: Phase,
    /// Bumped on every terminal write. Part of the delivery identity, so a
    /// corrected outcome for the same attempt is a genuinely new delivery.
    pub revision: u64,
    pub launched_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub result_path: Option<PathBuf>,
    #[serde(default)]
    pub summary: Option<String>,
    /// Delivery identities this record has already PUBLISHED.
    #[serde(default)]
    pub published: Vec<String>,
    /// Delivery identities a consumer has already CONSUMED. The two lists are
    /// separate on purpose: a publication that never reached anyone and one
    /// that was read are different facts.
    #[serde(default)]
    pub consumed: Vec<String>,
    #[serde(default)]
    pub queued: Vec<QueuedMessage>,
    /// `(provider, reservation id)` for the token reservation this delegation
    /// holds, when it took one.
    #[serde(default)]
    pub reservation: Option<(String, String)>,
    /// The canonical checkout this delegation holds an exclusive write claim
    /// on, when `--mode writing` gave it one.
    #[serde(default)]
    pub write_claim: Option<PathBuf>,
    #[serde(default)]
    pub cancel_requested: bool,
    /// Tool executions whose effect may or may not have happened (issue
    /// #478's `OutcomeUnknown`). Preserved verbatim by [`close`]: an
    /// unresolved effect is exactly the thing a cancel must not erase.
    #[serde(default)]
    pub unknown_tool_outcomes: Vec<String>,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
}

impl Record {
    /// The delivery identity for this record's CURRENT attempt/revision.
    pub fn identity(&self) -> String {
        delivery_identity(&self.handle.delegation, self.handle.attempt, self.revision)
    }
}

/// `<delegation>:<attempt>:<revision>`. The only identity a consumer
/// deduplicates on -- never a mail file name (which the mailbox may reuse)
/// and never a provider conversation id (which a resume changes).
pub fn delivery_identity(delegation: &str, attempt: u32, revision: u64) -> String {
    format!("{delegation}:{attempt}:{revision}")
}

fn dir(state: &StateDir, repo: &Path) -> PathBuf {
    state.delegations().join(repo_slug(repo))
}

fn record_path(state: &StateDir, repo: &Path, delegation: &str) -> PathBuf {
    dir(state, repo).join(format!("{delegation}.json"))
}

/// Rejects anything that is not a plain, path-safe id, so a delegation id
/// that arrived as untrusted provider output can never name a file outside
/// this repository's own delegation directory.
fn validate_id(delegation: &str) -> CtxResult<()> {
    if delegation.is_empty()
        || delegation.len() > 128
        || !delegation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "delegation id {delegation:?} must be 1-128 characters of [A-Za-z0-9_-]"
        )
        .into());
    }
    Ok(())
}

fn lock_path(state: &StateDir, repo: &Path, delegation: &str) -> PathBuf {
    dir(state, repo).join(format!("{delegation}.lock"))
}

/// One advisory OS lock per delegation record, mirroring `group::lock_group`
/// exactly (same `open_lock_file`, same per-record granularity, same "leave
/// the file behind on drop" reasoning). Every read-modify-write below
/// acquires this BEFORE its own [`load`] and holds it through the matching
/// [`save`], so two concurrent mutators of the SAME record (`publish_
/// terminal` racing `interrupt`, or two sweeps) can never lose one's update
/// to the other's stale-read overwrite -- `task.rs`'s `lock_tasks` gives its
/// own event log the identical guarantee.
struct DelegationLock(std::fs::File);

impl Drop for DelegationLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock_delegation(state: &StateDir, repo: &Path, delegation: &str) -> CtxResult<DelegationLock> {
    create_private_dir_all(&dir(state, repo))?;
    let file = super::group::open_lock_file(&lock_path(state, repo, delegation))?;
    file.lock()?;
    Ok(DelegationLock(file))
}

/// Writes `record` to disk. Mirrors `group::create`'s private-dir-then-
/// atomic-write shape exactly.
pub fn save(state: &StateDir, repo: &Path, record: &Record) -> CtxResult<()> {
    validate_id(&record.handle.delegation)?;
    create_private_dir_all(&dir(state, repo))?;
    let json = serde_json::to_string_pretty(record)?;
    write_private(&record_path(state, repo, &record.handle.delegation), &json)?;
    Ok(())
}

/// `None` both for a missing file and for one that fails to parse -- a
/// caller cannot tell those apart anyway, and a malformed record is left on
/// disk rather than destroyed to make a read succeed (`group::load`'s rule).
pub fn load(state: &StateDir, repo: &Path, delegation: &str) -> Option<Record> {
    let contents = std::fs::read_to_string(record_path(state, repo, delegation)).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Every delegation currently on disk for `repo`, newest launch first. A
/// file that fails to parse is skipped, never fatal to the listing.
pub fn list(state: &StateDir, repo: &Path) -> Vec<Record> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir(state, repo)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(record) = serde_json::from_str::<Record>(&contents) {
                found.push(record);
            }
        }
    }
    found.sort_by(|a, b| {
        b.launched_at
            .cmp(&a.launched_at)
            .then_with(|| a.handle.delegation.cmp(&b.handle.delegation))
    });
    found
}

/// The immediate, durable launch receipt: written before the worker this
/// handle names has done anything at all, so an orchestrator that crashes a
/// millisecond later still finds the delegation it started.
pub fn record_launch(
    state: &StateDir,
    repo: &Path,
    handle: WorkerHandle,
    parent_session: Option<String>,
    now: u64,
) -> CtxResult<Record> {
    validate_id(&handle.delegation)?;
    let attempt = Attempt {
        attempt: handle.attempt,
        runtime: handle.runtime,
        worker_session: handle.worker_session.clone(),
        short: handle.short.clone(),
        started_at: now,
        ended_at: None,
        phase: Phase::Launched,
        exit_code: None,
        result_path: None,
        summary: None,
    };
    let record = Record {
        schema_version: SCHEMA_VERSION,
        handle,
        parent_session,
        phase: Phase::Launched,
        revision: 0,
        launched_at: now,
        updated_at: now,
        exit_code: None,
        result_path: None,
        summary: None,
        published: Vec::new(),
        consumed: Vec::new(),
        queued: Vec::new(),
        reservation: None,
        write_claim: None,
        cancel_requested: false,
        unknown_tool_outcomes: Vec::new(),
        attempts: vec![attempt],
    };
    save(state, repo, &record)?;
    Ok(record)
}

/// Records the ownership this delegation actually took, so [`close`] can
/// release exactly that and nothing else.
pub fn record_ownership(
    state: &StateDir,
    repo: &Path,
    delegation: &str,
    reservation: Option<(String, String)>,
    write_claim: Option<PathBuf>,
    now: u64,
) -> CtxResult<()> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    record.reservation = reservation;
    record.write_claim = write_claim;
    record.updated_at = now;
    save(state, repo, &record)
}

/// What one [`publish_terminal`] call actually did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub identity: String,
    /// `false` when this exact identity had already been published, i.e. the
    /// call was a replay of a publication that already happened.
    pub published: bool,
    /// Whether the mail transport accepted the notification. `false` is not a
    /// failure of the publication: the outcome is already durable and a later
    /// sweep can retry it.
    pub mailed: bool,
}

/// Persists the terminal outcome and THEN notifies, in that order.
///
/// Idempotent by construction: the identity is computed from the attempt and
/// the post-write revision, and a second call that would produce an identity
/// already in `published` neither bumps the revision again nor sends a second
/// mail. A caller that crashed between the durable write and the mail calls
/// this again and gets the mail without a second outcome.
/// Nine arguments, over clippy's default seven: each is an independent fact
/// about one publication (where state lives, which repository, the operator
/// config the mail service needs, which delegation, and the four terminal
/// facts plus the clock). Bundling them into a struct would move the same
/// list one level down without making any call site clearer -- the same
/// reasoning `agent::append_execution_segments` already records.
#[allow(clippy::too_many_arguments)]
pub fn publish_terminal(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    delegation: &str,
    phase: Phase,
    exit_code: Option<i32>,
    summary: Option<String>,
    result_path: Option<PathBuf>,
    now: u64,
) -> CtxResult<Publication> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };

    // A replay: the same attempt already reached the same terminal facts.
    let already = record.phase == phase
        && record.exit_code == exit_code
        && record.summary == summary
        && record.result_path == result_path
        && record.phase.is_terminal();
    if !already {
        record.phase = phase;
        record.exit_code = exit_code;
        record.summary.clone_from(&summary);
        record.result_path.clone_from(&result_path);
        record.revision = record.revision.saturating_add(1);
        record.updated_at = now;
        if let Some(attempt) = record.attempts.last_mut() {
            attempt.phase = phase;
            attempt.exit_code = exit_code;
            attempt.ended_at = Some(now);
            attempt.summary.clone_from(&summary);
            attempt.result_path.clone_from(&result_path);
        }
        save(state, repo, &record)?;
    }

    let identity = record.identity();
    if record.published.iter().any(|seen| seen == &identity) {
        return Ok(Publication {
            identity,
            published: false,
            mailed: false,
        });
    }

    // Only a SUCCESSFUL mail earns the identity a place in `published`: the
    // `Publication` doc above promises a failed mail is retryable, and the
    // only thing that makes it retryable is this exact identity staying
    // absent from `published` so a later call (the `drain_all` sweep, or a
    // caller retrying after a crash) takes the branch above unchanged and
    // tries `notify_parent` again rather than treating it as already sent.
    let mailed = notify_parent(state, repo, cfg, &record, &identity).is_ok();
    if mailed {
        record.published.push(identity.clone());
        record.updated_at = now;
        save(state, repo, &record)?;
    }
    Ok(Publication {
        identity,
        published: true,
        mailed,
    })
}

/// The parent-facing notification body. Deliberately a POINTER plus the
/// bounded facts (#454): outcome, exit code, delivery identity and where the
/// full report lives -- never the worker's own transcript.
fn notify_parent(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    record: &Record,
    identity: &str,
) -> CtxResult<PathBuf> {
    let slug = repo_slug(repo);
    let mut body = format!(
        "zirv delegation {} ({} runtime) {}",
        record.handle.delegation,
        record.handle.runtime.as_str(),
        record.phase.as_str()
    );
    if let Some(code) = record.exit_code {
        body.push_str(&format!(" (exit {code})"));
    }
    if let Some(task) = &record.handle.task {
        body.push_str(&format!("\ntask: {task}"));
    }
    if let Some(summary) = &record.summary {
        body.push_str(&format!("\nsummary: {summary}"));
    }
    if let Some(path) = &record.result_path {
        body.push_str(&format!("\nfull report: {}", path.display()));
    }
    body.push_str(&format!("\ndelivery: {identity}"));
    let msg = super::mail::Message {
        from_session: record.handle.worker_session.clone(),
        from_agent: record.handle.runtime.as_str().to_string(),
        to: "any".to_string(),
        to_session: record.parent_session.clone(),
        sent: record.updated_at,
        body,
    };
    super::mail::store_to(state, &slug, &slug, &msg, cfg)
}

/// The consumer half of at-least-once delivery. `true` exactly once per
/// identity; every replay of the same identity is `false`, which is what
/// lets a duplicated transport delivery be dropped without losing a genuinely
/// new outcome that carries a different revision.
pub fn consume_delivery(
    state: &StateDir,
    repo: &Path,
    delegation: &str,
    identity: &str,
) -> CtxResult<bool> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    if record.consumed.iter().any(|seen| seen == identity) {
        return Ok(false);
    }
    record.consumed.push(identity.to_string());
    save(state, repo, &record)?;
    Ok(true)
}

/// What [`send`] did with one message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dispatch {
    Delivered {
        path: PathBuf,
    },
    /// The target session has an attention latch open -- a permission dialog,
    /// a question, a quota park. The message is durably queued and
    /// [`drain_queued`] retries it at the next boundary where the latch is
    /// gone. It is NEVER typed at the dialog (issue #468).
    Queued {
        id: String,
        reason: String,
    },
}

/// Directed message to a delegation's current worker, through the shared mail
/// service. Blocked-at-this-boundary messages are queued rather than dropped
/// or forced.
pub fn send(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    delegation: &str,
    body: &str,
    now: u64,
) -> CtxResult<Dispatch> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    let status = super::attention::load(state, &record.handle.short);
    if let Some(attention) = super::attention::blocking(&status) {
        let reason = super::attention::block_reason(attention).to_string();
        let id = format!("{}-{}", record.handle.delegation, record.queued.len() + 1);
        record.queued.push(QueuedMessage {
            id: id.clone(),
            body: body.to_string(),
            queued_at: now,
            reason: reason.clone(),
        });
        record.updated_at = now;
        save(state, repo, &record)?;
        log_boundary(state, &record, "delegation-send-deferred", &reason, &id);
        return Ok(Dispatch::Queued { id, reason });
    }
    let path = deliver_now(state, repo, cfg, &record, body, now)?;
    Ok(Dispatch::Delivered { path })
}

fn deliver_now(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    record: &Record,
    body: &str,
    now: u64,
) -> CtxResult<PathBuf> {
    let slug = repo_slug(repo);
    let msg = super::mail::Message {
        from_session: record
            .parent_session
            .clone()
            .unwrap_or_else(|| "orchestrator".to_string()),
        from_agent: "zirv".to_string(),
        to: "any".to_string(),
        to_session: Some(record.handle.short.clone()),
        sent: now,
        body: body.to_string(),
    };
    super::mail::store_to(state, &slug, &slug, &msg, cfg)
}

/// Retries every queued message at THIS boundary. Delivers nothing while the
/// latch is still open; drains in FIFO order once it is gone, and each queued
/// message is delivered at most once because it is removed from the record
/// only after its own delivery succeeded.
pub fn drain_queued(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    delegation: &str,
    now: u64,
) -> CtxResult<Vec<Dispatch>> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    if record.queued.is_empty() {
        return Ok(Vec::new());
    }
    let status = super::attention::load(state, &record.handle.short);
    if let Some(attention) = super::attention::blocking(&status) {
        let reason = super::attention::block_reason(attention).to_string();
        return Ok(record
            .queued
            .iter()
            .map(|queued| Dispatch::Queued {
                id: queued.id.clone(),
                reason: reason.clone(),
            })
            .collect());
    }
    let mut delivered = Vec::new();
    let pending = std::mem::take(&mut record.queued);
    for queued in pending {
        match deliver_now(state, repo, cfg, &record, &queued.body, now) {
            Ok(path) => {
                log_boundary(
                    state,
                    &record,
                    "delegation-send-delivered",
                    &queued.reason,
                    &queued.id,
                );
                delivered.push(Dispatch::Delivered { path });
            }
            Err(_) => {
                // Transport failed: keep it queued rather than losing it.
                record.queued.push(queued);
            }
        }
    }
    record.updated_at = now;
    save(state, repo, &record)?;
    Ok(delivered)
}

/// One decision-log row per deferral and per eventual delivery, both
/// carrying the same message id -- issue #468's diagnosability requirement,
/// applied to delegation messages as well as pane advisories. Best-effort:
/// a logging failure never changes whether a message is delivered.
fn log_boundary(state: &StateDir, record: &Record, action: &str, reason: &str, id: &str) {
    let detail = format!("{id} ({})", record.handle.delegation);
    let _ = super::log::append(
        state,
        &super::log::Decision {
            ts: record.updated_at,
            session: &record.handle.short,
            verb: "delegation",
            verdict: reason,
            score: 0,
            action,
            detail: &detail,
            observed_at: None,
        },
    );
}

/// What a bounded [`wait`] observed. Never wakes a model: it is a read of
/// durable state plus a deadline comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Ready(Box<Record>),
    Pending,
    TimedOut,
}

pub fn wait(
    state: &StateDir,
    repo: &Path,
    delegation: &str,
    now: u64,
    deadline: u64,
) -> CtxResult<WaitOutcome> {
    let Some(record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    if record.phase.is_terminal() {
        return Ok(WaitOutcome::Ready(Box::new(record)));
    }
    if now >= deadline {
        return Ok(WaitOutcome::TimedOut);
    }
    Ok(WaitOutcome::Pending)
}

/// A bounded result manifest (#454): enough to decide what to do next, plus
/// explicit references to everything it does NOT contain. Never the worker's
/// transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub delegation: String,
    pub attempt: u32,
    pub runtime: &'static str,
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `true` when `summary` was cut to the byte budget: a consumer must be
    /// able to tell what it has not seen.
    pub summary_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_path: Option<PathBuf>,
    /// Delivery identities already published for this delegation.
    pub deliveries: Vec<String>,
    pub unknown_tool_outcomes: Vec<String>,
    pub queued_messages: usize,
    pub continuation: &'static str,
}

/// Caps `text` at `max_bytes` on a char boundary, reporting whether it cut.
fn cap(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

pub fn result(
    state: &StateDir,
    repo: &Path,
    delegation: &str,
    max_summary_bytes: usize,
) -> CtxResult<Manifest> {
    let Some(record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    let (summary, summary_truncated) = match &record.summary {
        Some(text) => {
            let (capped, truncated) = cap(text, max_summary_bytes);
            (Some(capped), truncated)
        }
        None => (None, false),
    };
    Ok(Manifest {
        schema_version: SCHEMA_VERSION,
        delegation: record.handle.delegation.clone(),
        attempt: record.handle.attempt,
        runtime: record.handle.runtime.as_str(),
        phase: record.phase.as_str(),
        task: record.handle.task.clone(),
        exit_code: record.exit_code,
        summary,
        summary_truncated,
        result_path: record.result_path.clone(),
        deliveries: record.published.clone(),
        unknown_tool_outcomes: record.unknown_tool_outcomes.clone(),
        queued_messages: record.queued.len(),
        continuation: continuation_label(&record),
    })
}

fn continuation_label(record: &Record) -> &'static str {
    if !record.phase.is_terminal() {
        return "directed";
    }
    match record.handle.runtime {
        RuntimeKind::Native => "resume",
        _ => "checkpoint",
    }
}

/// How a follow-up actually reaches the ORIGINAL worker. There is no
/// "most recent session" fallback anywhere in this enum: an unknown
/// delegation id is an error, and a worker with no supported resume path
/// reports a transparent checkpoint rather than pretending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Continuation {
    /// The worker is still live: the follow-up goes through the same directed
    /// mail service [`send`] uses, to that worker's own session.
    Directed { dispatch: Dispatch },
    /// A finished NATIVE worker: the journal is the conversation, so a
    /// continuation attempt resumes the same journal session. The attempt is
    /// appended; the prior attempt's outcome is preserved untouched.
    Resume {
        journal_session: String,
        attempt: u32,
    },
    /// No verified resume path (a finished legacy worker whose adapter cannot
    /// resume headlessly). The caller gets the bounded handoff it would need
    /// to launch a REPLACEMENT, explicitly marked as one -- it has no claim on
    /// the original worker's hidden context.
    Checkpoint { handoff: String },
}

pub fn follow_up(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    delegation: &str,
    body: &str,
    now: u64,
) -> CtxResult<Continuation> {
    // An unlocked peek: this function only ever MUTATES the record under
    // `lock_delegation` (the Resume branch, below), and never while `send`
    // (which takes its own lock) is also running -- taking the lock here
    // too, before delegating to `send`, would self-deadlock on the same
    // non-reentrant file lock.
    let Some(record) = load(state, repo, delegation) else {
        return Err(format!(
            "no delegation {delegation:?} in this repository; a follow-up is addressed to the \
             delegation it continues, never to whichever session ran most recently"
        )
        .into());
    };
    if !record.phase.is_terminal() {
        let dispatch = send(state, repo, cfg, delegation, body, now)?;
        return Ok(Continuation::Directed { dispatch });
    }
    if record.handle.runtime == RuntimeKind::Native && record.phase != Phase::Closed {
        let _lock = lock_delegation(state, repo, delegation)?;
        let Some(mut record) = load(state, repo, delegation) else {
            return Err(format!("no delegation {delegation:?} in this repository").into());
        };
        let journal_session = record.handle.worker_session.clone();
        let attempt = record.handle.attempt.saturating_add(1);
        record.handle.attempt = attempt;
        record.phase = Phase::Launched;
        record.exit_code = None;
        record.updated_at = now;
        record.attempts.push(Attempt {
            attempt,
            runtime: RuntimeKind::Native,
            worker_session: journal_session.clone(),
            short: record.handle.short.clone(),
            started_at: now,
            ended_at: None,
            phase: Phase::Launched,
            exit_code: None,
            result_path: None,
            summary: None,
        });
        save(state, repo, &record)?;
        return Ok(Continuation::Resume {
            journal_session,
            attempt,
        });
    }
    let mut handoff = format!(
        "REPLACEMENT worker for delegation {} (attempt {}). This is not the original worker and \
         has none of its hidden context.",
        record.handle.delegation, record.handle.attempt
    );
    if let Some(task) = &record.handle.task {
        handoff.push_str(&format!("\ntask: {task}"));
    }
    if let Some(path) = &record.result_path {
        handoff.push_str(&format!("\nprior report: {}", path.display()));
    }
    handoff.push_str(&format!("\nfollow-up: {body}"));
    Ok(Continuation::Checkpoint { handoff })
}

/// Requests cancellation. Marks the record, but never claims an already-
/// started effect was undone: an in-flight tool that cannot be cancelled
/// stays an unknown outcome (issue #478's own contract), which is why this
/// never clears `unknown_tool_outcomes`.
pub fn interrupt(state: &StateDir, repo: &Path, delegation: &str, now: u64) -> CtxResult<Record> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    record.cancel_requested = true;
    record.updated_at = now;
    save(state, repo, &record)?;
    Ok(record)
}

/// Releases the ownership this delegation took -- its provider reservation
/// and its worktree write claim -- and retires the record. Receipts
/// (`published`, `consumed`, `attempts`) and `unknown_tool_outcomes` are
/// deliberately preserved: closing a delegation must never make it look as
/// though its outcome was never delivered, or as though an effect whose
/// result nobody knows definitely did not happen.
pub fn close(state: &StateDir, repo: &Path, delegation: &str, now: u64) -> CtxResult<Record> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    if !record.phase.is_terminal() {
        return Err(format!(
            "delegation {delegation:?} is still running; wait for its terminal acknowledgement before closing"
        )
        .into());
    }
    if let Some((provider, id)) = record.reservation.take() {
        let _ = super::reservation::release(state, &provider, &id);
    }
    // The writer permit itself is an RAII guard held by the running process;
    // the claim recorded here is released with it. Dropping the recorded
    // claim is what lets a later delegation take the same tree.
    record.write_claim = None;
    record.phase = Phase::Closed;
    record.updated_at = now;
    save(state, repo, &record)?;
    Ok(record)
}

/// The marker line [`notify_parent`] writes into every terminal
/// notification. A consumer reads the delivery identity back out of it
/// structurally rather than parsing prose.
pub const DELIVERY_LINE_PREFIX: &str = "delivery: ";

/// The delivery identity carried by `body`, if any.
pub fn delivery_of(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix(DELIVERY_LINE_PREFIX))
        .map(|identity| identity.trim().to_string())
        .filter(|identity| !identity.is_empty())
}

/// A read-only PEEK at whether `identity` names a delegation outcome this
/// repository has ALREADY consumed -- never mutates anything, unlike
/// [`consume_delivery`]/[`mark_delivery_consumed`].
///
/// `mail.rs`'s inbox rendering uses this to drop a message whose delivery was
/// consumed in an EARLIER call, while deliberately NOT consuming anything
/// itself: consuming every candidate up front, before the byte-cap decides
/// which of them are actually rendered, used to mark a message the cap only
/// DEFERRED to `more_unread` as consumed anyway -- so the next call dropped
/// it as a false duplicate, having never actually shown it (review finding
/// on issue #479's inbox rendering).
///
/// Fails open on purpose: an identity that names no delegation record here
/// (an outcome from another repository, a hand-written line, a record swept
/// away) is reported as not-yet-consumed. Hiding a message nobody can
/// account for would turn a bookkeeping gap into lost mail, which is the
/// failure this whole mechanism exists to prevent.
pub fn is_delivery_consumed(state: &StateDir, repo: &Path, identity: &str) -> bool {
    let Some(delegation) = identity.split(':').next() else {
        return false;
    };
    let Some(record) = load(state, repo, delegation) else {
        return false;
    };
    record.consumed.iter().any(|seen| seen == identity)
}

/// The durable half of the split [`is_delivery_consumed`] started: marks
/// `identity` consumed for a message that has actually been rendered to a
/// caller. Never call this for a message the byte-cap deferred to
/// `more_unread` -- only for one this call is actually handing over, or the
/// NEXT call will wrongly drop it as a duplicate. Best-effort: a bookkeeping
/// failure here must never fail the read that already succeeded.
pub fn mark_delivery_consumed(state: &StateDir, repo: &Path, identity: &str) {
    let Some(delegation) = identity.split(':').next() else {
        return;
    };
    let _ = consume_delivery(state, repo, delegation, identity);
}

/// Retries every delegation's deferred messages at THIS boundary, and reports
/// how many were actually delivered. Called from the orchestrator-side
/// checkpoint (`zirv ctx inbox`), which is by construction a moment no
/// approval dialog is open on the caller -- the #468 rule, applied per
/// worker rather than per pane.
///
/// Also the retry path [`publish_terminal`]'s own doc comment promises: a
/// terminal record whose current delivery identity is still absent from
/// `published` had its mail fail (or never ran at all), and `publish_
/// terminal` is idempotent by construction -- calling it again with the
/// record's own already-durable terminal facts changes nothing but the
/// delivery outcome, so a transport failure is retried here and, once it
/// succeeds, delivered exactly once (review finding on issue #479's
/// `publish_terminal`).
pub fn drain_all(state: &StateDir, repo: &Path, cfg: &CtxConfig, now: u64) -> usize {
    let mut delivered = 0;
    for record in list(state, repo) {
        if !record.queued.is_empty()
            && let Ok(dispatches) = drain_queued(state, repo, cfg, &record.handle.delegation, now)
        {
            delivered += dispatches
                .iter()
                .filter(|dispatch| matches!(dispatch, Dispatch::Delivered { .. }))
                .count();
        }
        if record.phase.is_terminal() && !record.published.contains(&record.identity()) {
            let retried = publish_terminal(
                state,
                repo,
                cfg,
                &record.handle.delegation,
                record.phase,
                record.exit_code,
                record.summary.clone(),
                record.result_path.clone(),
                now,
            );
            if matches!(retried, Ok(publication) if publication.mailed) {
                delivered += 1;
            }
        }
    }
    delivered
}

// -- the launch seam -----------------------------------------------------

/// One request to actually START a worker, independent of which runtime will
/// run it. Everything here is data a parent can legitimately ask for; nothing
/// is a grant (`mode`, `path_scope` and the rest are narrowed against the
/// parent's own envelope by `agent::run_with`, never widened by asking).
#[derive(Clone, Debug)]
pub struct LaunchRequest {
    pub runtime: RuntimeKind,
    /// The harness name (harness runtime) or the provider route (native).
    pub target: String,
    pub brief: String,
    pub role: String,
    pub task: Option<String>,
    pub group: Option<String>,
    pub workdir: Option<PathBuf>,
    /// Issue #541 chunk C, decision 2: the workflow agent manifest id this
    /// delegation names. `None` defers to the role's own default manifest
    /// (`team::default_manifest_for_role`); a role outside the closed team
    /// (`worker`, `seat`, an operator's own label) skips the manifest/
    /// team-plan checks entirely regardless of this field.
    pub manifest: Option<String>,
    /// Issue #541 chunk C, decision 2: bypass the "must match an unfilled
    /// team-plan seat" rule. Only the COORDINATOR seat's request for this is
    /// ever honoured (`coordinator::check`); anyone else's is silently
    /// ignored rather than erroring, since asking is not itself a violation.
    pub plan_override_requested: bool,
    /// Issue #541 chunk C follow-up: the CALLER's own agent registry (built-
    /// ins plus, where the caller resolved them, operator-global/repository
    /// manifests), used ONLY to answer "is `manifest` (or the role's own
    /// default) a known manifest, and what does it grant". `delegate` itself
    /// must stay pure -- no filesystem read, no home-directory lookup -- so
    /// it never builds this registry on its own. `None` falls back to
    /// `delegate`'s own built-in-only lookup, exactly as before this field
    /// existed: every caller that does not yet plumb a registry through (and
    /// every non-`HomeGuard` test) keeps working unchanged.
    pub manifest_registry: Option<std::sync::Arc<crate::commands::workflow::agents::AgentRegistry>>,
    pub read_only: bool,
    pub budget_tokens: Option<u64>,
    pub max_tool_calls: Option<u32>,
    /// Conversation identity assigned by the delegation service, never model input.
    pub worker_session: Option<String>,
    /// Remaining depth granted by the coordinator's bounds check.
    pub delegated_depth: Option<u8>,
    /// The delegating native session's installed envelope. Production launchers
    /// overlay this on their process environment before resolving the child.
    pub parent_envelope: Option<String>,
    pub parent_principal: Option<String>,
    /// Live cancellation observed by both native and wrapped launchers.
    pub cancellation: std::sync::Arc<super::provider::adapter::CancellationFlag>,
}

/// What a launcher observed. `receipt` is the delegating command's own
/// `--json` [`crate::commands::ctx::agent::DelegationReceipt`] text when one
/// was produced -- never the worker's transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchedWorker {
    pub exit_code: i32,
    pub session: String,
    pub short: String,
    pub receipt: Option<String>,
}

/// How a native session actually starts a worker.
///
/// The production implementation is [`AgentLauncher`], which calls
/// `agent::run_with` -- the exact function `zirv agent` itself runs, so the
/// native `delegate` tool and the CLI verb are one code path with one set of
/// gates, not two that can drift. The seam exists so a deterministic test can
/// substitute a launcher that starts nothing.
pub trait WorkerLauncher: std::fmt::Debug + Send {
    fn launch(&mut self, request: &LaunchRequest) -> CtxResult<LaunchedWorker>;
}

/// The production launcher: one `agent::run_with` call, under `--json` so the
/// receipt comes back structured rather than scraped out of human lines.
#[derive(Debug, Default)]
pub struct AgentLauncher {
    pub repo: PathBuf,
}

impl WorkerLauncher for AgentLauncher {
    fn launch(&mut self, request: &LaunchRequest) -> CtxResult<LaunchedWorker> {
        let args = super::agent::AgentArgs {
            name: request.target.clone(),
            prompt: request.brief.clone(),
            role: Some(request.role.clone()),
            group: request.group.clone(),
            task: request.task.clone(),
            workdir: request.workdir.clone(),
            budget_tokens: request.budget_tokens,
            max_tool_calls: request.max_tool_calls,
            mode: if request.read_only {
                super::permit::WorkerMode::ReadOnly
            } else {
                super::permit::WorkerMode::Writing
            },
            json: true,
            runtime: request.runtime.to_string(),
            session_id: request.worker_session.clone(),
            cancellation: Some(request.cancellation.clone()),
            depth: request.delegated_depth,
            ..Default::default()
        };
        let mut out: Vec<u8> = Vec::new();
        let process_env = super::config::env_from_process();
        let launch_env = super::agent::envelope_env(
            &process_env,
            request.parent_envelope.clone(),
            request.parent_principal.clone(),
        );
        let code = super::agent::run_with(&args, &mut out, &self.repo, &launch_env)?;
        let receipt = String::from_utf8_lossy(&out).trim().to_string();
        let parsed = serde_json::from_str::<serde_json::Value>(&receipt).ok();
        let session = parsed
            .as_ref()
            .and_then(|value| value.get("session"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(LaunchedWorker {
            exit_code: code,
            short: super::sessions::short_id(&session),
            session,
            receipt: (!receipt.is_empty()).then_some(receipt),
        })
    }
}

fn bind_launched_worker(
    state: &StateDir,
    repo: &Path,
    delegation: &str,
    worker: &LaunchedWorker,
    now: u64,
) -> CtxResult<Record> {
    let _lock = lock_delegation(state, repo, delegation)?;
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    if !worker.session.is_empty() {
        record.handle.worker_session.clone_from(&worker.session);
    }
    if !worker.short.is_empty() {
        record.handle.short.clone_from(&worker.short);
    }
    if let Some(attempt) = record.attempts.last_mut() {
        attempt
            .worker_session
            .clone_from(&record.handle.worker_session);
        attempt.short.clone_from(&record.handle.short);
    }
    record.updated_at = now;
    save(state, repo, &record)?;
    Ok(record)
}

fn launch_is_acknowledgement(worker: &LaunchedWorker) -> bool {
    worker
        .receipt
        .as_deref()
        .and_then(|receipt| serde_json::from_str::<serde_json::Value>(receipt).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|state| state == "launched")
}

/// A [`WorkerLauncher`] that starts nothing and records what it was asked
/// for. Lets the whole delegation tool surface be driven deterministically --
/// no provider, no harness, no child process -- while still going through the
/// real service methods.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct RecordingLauncher {
    pub launches: std::sync::Arc<std::sync::Mutex<Vec<LaunchRequest>>>,
    pub exit_code: i32,
}

#[cfg(test)]
impl WorkerLauncher for RecordingLauncher {
    fn launch(&mut self, request: &LaunchRequest) -> CtxResult<LaunchedWorker> {
        if let Ok(mut launches) = self.launches.lock() {
            launches.push(request.clone());
        }
        Ok(LaunchedWorker {
            exit_code: self.exit_code,
            session: "fixture-worker".to_string(),
            short: "fixture1".to_string(),
            receipt: Some(format!(
                "{{\"state\":\"reported\",\"exit_code\":{}}}",
                self.exit_code
            )),
        })
    }
}

/// The DELEGATING seat, as the launch path needs to know it.
///
/// Issue #485 (roadmap N16): `role` and `depth` are what make a delegation's
/// bounds decidable before anything starts. Both come from trusted runtime
/// state -- the role off the persisted seat record (reachable at effect time
/// as `ExecutionIdentity::role`), the depth off this session's own
/// `envelope::WorkerEnvelope` -- and neither is ever supplied by model
/// output.
#[derive(Clone, Copy, Debug)]
pub struct Parent<'a> {
    pub session: Option<&'a str>,
    pub short: &'a str,
    pub role: &'a str,
    pub depth: u8,
    /// Issue #488: the seat generation this delegator believes it holds,
    /// from the same trusted runtime state `role` comes from
    /// (`ExecutionIdentity::generation`). `None` for a caller with no seat
    /// generation to present at all -- a manual CLI delegation, a worker
    /// running outside any seat -- which is not fenced, exactly as
    /// `seat::fence` leaves an unseated process alone.
    pub generation: Option<u64>,
    /// The caller already holds this generation's seat lock across the whole effect.
    pub generation_locked: bool,
}

/// Registers one delegation and starts its worker, in that order.
///
/// The launch receipt is durable BEFORE `launcher` is called, so a crash
/// inside the launch still leaves a record naming the work that may have
/// started -- the opposite order would lose it. The terminal outcome is then
/// published through [`publish_terminal`], which is where the delivery
/// identity a consumer deduplicates on comes from.
///
/// Issue #485: the bounds decision (`coordinator::check`) happens FIRST, so
/// a refused delegation leaves no launch receipt naming work nobody started,
/// and the child's write posture is the one its own ROLE grants rather than
/// the one the caller asked for. Everything the check deliberately does not
/// cover -- the task claim, the writer permit, the group's child limit and
/// token budget, the per-provider reservation -- is enforced centrally
/// further down `agent::run_with`, which both runtimes go through.
pub fn delegate(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    launcher: &mut dyn WorkerLauncher,
    request: &LaunchRequest,
    parent: &Parent<'_>,
    now: u64,
) -> CtxResult<(Record, Option<Publication>)> {
    // Issue #488 (item 4): a superseded generation may not start work, and a
    // successor whose rollover is prepared but not yet committed may not
    // either. This is FIRST -- ahead of the bounds check and far ahead of the
    // durable launch receipt -- because a stale delegator that gets as far as
    // a receipt has already named work the live generation knows nothing
    // about. The refusal is `seat::StaleGeneration`, so a caller can tell
    // "you were replaced" from "your transaction has not committed yet"
    // without matching on prose.
    if let Some(generation) = parent.generation {
        super::seat::guard(state, parent.short, generation)?;
    }
    let mut graph = super::coordinator::load(state, repo);

    // Issue #541 chunk C, decision 2: resolve the manifest identity and
    // team-plan facts the pure `coordinator::check` needs, from trusted
    // runtime state -- a registry lookup and a plan read are both I/O, so
    // they happen HERE, never inside `check` itself. Chunk C follow-up: the
    // registry itself is never built here -- when the caller supplied one
    // (`request.manifest_registry`, resolved with the operator's home
    // directory and this repository's own layer), an operator-global or
    // repository manifest is honoured exactly as `team_plan`/the slash
    // commands already honour it; a caller that has not plumbed one through
    // falls back to the built-in-only lookup this had before, so `delegate`
    // still never reads a filesystem or a home directory on its own.
    let team_role_requested = super::team::TeamRole::parse(&request.role);
    let requested_manifest_id: Option<String> = team_role_requested.and_then(|role| {
        request
            .manifest
            .clone()
            .or_else(|| super::team::default_manifest_for_role(role).map(str::to_string))
    });
    let manifest_facts = requested_manifest_id.as_ref().and_then(|id| {
        let owned_fallback;
        let registry = match &request.manifest_registry {
            Some(registry) => registry.as_ref(),
            None => {
                owned_fallback = crate::commands::workflow::agents::AgentRegistry::load(
                    repo, None, false, false,
                )
                .ok()?;
                &owned_fallback
            }
        };
        let agent = registry.get(id).ok()?;
        Some(super::coordinator::ManifestFacts {
            team_role: crate::commands::workflow::agents::team_role_for(&agent.manifest),
            may_write: !agent.manifest.read_only,
        })
    });
    let manifest_bounds =
        requested_manifest_id
            .as_deref()
            .map(|id| super::coordinator::ManifestBounds {
                requested_id: id,
                known: manifest_facts,
            });

    let resolved_plan = super::coordinator::resolve_team_plan(state, repo, &graph);
    let empty_paths: Vec<String> = Vec::new();
    let matched_seat = resolved_plan.as_ref().and_then(|plan| {
        let task_id = request.task.as_deref()?;
        plan.seats.iter().find(|seat| {
            seat.id == task_id
                && requested_manifest_id.as_deref() == Some(seat.manifest_id.as_str())
                && team_role_requested == Some(seat.team_role)
        })
    });
    let matching_unfilled_seat = matched_seat
        .filter(|seat| !graph.seat_filled(&seat.id))
        .map(|seat| seat.id.as_str());
    let matched_claim_paths: &[String] =
        matched_seat.map_or(&empty_paths, |seat| &seat.claim.paths);
    // Issue #541 chunk C review finding: `ancestors` are the seats
    // `matched_seat` transitively `depends_on` -- a planned HAND-OFF, never
    // a conflict, however wide their own claim is (a bug-fix plan's
    // `debugger-1` and `implementer-1` share a claim on purpose). And a
    // seat only holds its claim while IN FLIGHT (`seat_claim_active`, not
    // `seat_filled`): a settled seat -- `Completed`, `Failed`, `Cancelled`
    // -- has released it, which is what lets a sequential hand-off admit
    // its successor instead of refusing it as a permanent conflict.
    let ancestor_ids: std::collections::BTreeSet<&str> = match (&resolved_plan, matched_seat) {
        (Some(plan), Some(seat)) => plan.ancestors_of(&seat.id),
        _ => std::collections::BTreeSet::new(),
    };
    let active_claim_paths: Vec<&[String]> = resolved_plan
        .as_ref()
        .map(|plan| {
            plan.seats
                .iter()
                .filter(|seat| Some(seat.id.as_str()) != request.task.as_deref())
                .filter(|seat| !ancestor_ids.contains(seat.id.as_str()))
                .filter(|seat| graph.seat_claim_active(&seat.id))
                .map(|seat| seat.claim.paths.as_slice())
                .collect()
        })
        .unwrap_or_default();
    let plan_bounds = resolved_plan
        .as_ref()
        .map(|_| super::coordinator::PlanBounds {
            exists: true,
            matching_unfilled_seat,
            matched_claim_paths,
            active_claim_paths: &active_claim_paths,
            is_coordinator: parent.role == super::team::COORDINATOR,
            override_requested: request.plan_override_requested,
        });

    let grant = super::coordinator::check(&super::coordinator::Bounds {
        parent_role: parent.role,
        child_role: &request.role,
        depth: parent.depth,
        cancelled: graph.cancelled,
        requested_write: !request.read_only,
        manifest: manifest_bounds,
        plan: plan_bounds,
    })
    .map_err(|refusal| refusal.to_string())?;

    // The one field a role identity may narrow. Cloned rather than mutated
    // in place: the caller's request is what it asked for, and what actually
    // ran has to be readable as a separate fact.
    let delegation_id = uuid::Uuid::new_v4().simple().to_string();
    let worker_session = format!("{delegation_id}-worker");
    let cancellation = std::sync::Arc::new(super::provider::adapter::CancellationFlag::default());
    let request = &LaunchRequest {
        read_only: !grant.write,
        worker_session: Some(worker_session.clone()),
        delegated_depth: Some(grant.depth),
        cancellation: cancellation.clone(),
        ..request.clone()
    };

    let handle = WorkerHandle {
        delegation: delegation_id.clone(),
        attempt: 1,
        runtime: request.runtime,
        worker_session: worker_session.clone(),
        short: super::sessions::short_id(&worker_session),
        role: request.role.clone(),
        task: request.task.clone(),
        group: request.group.clone(),
        objective: None,
        workdir: request
            .workdir
            .clone()
            .unwrap_or_else(|| repo.to_path_buf()),
        manifest: requested_manifest_id.clone(),
        plan_override: grant.plan_override,
    };
    // The coordinator's own graph, written beside the launch receipt: which
    // task this delegation answers for, which role took it and on which
    // runtime. A crash between here and the outcome leaves a node a resumed
    // coordinator can still address, which is the whole point of persisting
    // it.
    let node = request
        .task
        .clone()
        .unwrap_or_else(|| delegation_id.clone());
    // Issue #488: the graph write goes through the FENCED door, and re-reads
    // rather than writing back the copy loaded above -- so the window in
    // which a concurrently committed rollover could be overwritten is the
    // fenced write itself rather than the whole launch. A caller with no
    // generation to present writes exactly as before.
    let dispatch = |graph: &mut super::coordinator::Coordinator| {
        graph.dispatched(
            &node,
            &request.role,
            request.runtime.as_str(),
            &delegation_id,
            now,
        );
    };
    let launched = match parent.generation {
        Some(generation) if !parent.generation_locked => {
            super::coordinator::update_fenced(state, repo, parent.short, generation, |graph| {
                let launched = record_launch(
                    state,
                    repo,
                    handle.clone(),
                    parent.session.map(str::to_string),
                    now,
                )?;
                dispatch(graph);
                Ok(launched)
            })?
        }
        Some(_) => {
            let launched =
                record_launch(state, repo, handle, parent.session.map(str::to_string), now)?;
            super::coordinator::update(state, repo, |graph| dispatch(graph))?;
            launched
        }
        None => {
            let launched =
                record_launch(state, repo, handle, parent.session.map(str::to_string), now)?;
            dispatch(&mut graph);
            // Review finding on issue #485: the launch receipt above is what
            // is authoritative, so a coordinator-graph store failure must
            // never block it -- but it must not vanish silently either. One
            // decision-log line, the same best-effort idiom `log_boundary`
            // uses just above.
            if let Err(error) = super::coordinator::store(state, repo, &graph) {
                let detail = format!("delegation {delegation_id}: {error}");
                let _ = super::log::append(
                    state,
                    &super::log::Decision {
                        ts: now,
                        session: parent.short,
                        verb: "delegation",
                        verdict: "error",
                        score: 0,
                        action: "coordinator-store-failed",
                        detail: &detail,
                        observed_at: None,
                    },
                );
            }
            launched
        }
    };

    let launch_guard = match (parent.generation, parent.generation_locked) {
        (_, true) => None,
        (Some(generation), false) => super::seat::lock_generation(state, parent.short, generation)?,
        (None, false) => None,
    };
    let watcher_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let state = state.clone();
        let repo = repo.to_path_buf();
        let delegation = delegation_id.clone();
        let cancellation = cancellation.clone();
        let done = watcher_done.clone();
        std::thread::spawn(move || {
            use super::provider::adapter::Cancellation as _;
            while !done.load(std::sync::atomic::Ordering::Acquire) && !cancellation.is_cancelled() {
                match load(&state, &repo, &delegation) {
                    Some(record) if record.cancel_requested => {
                        cancellation.cancel();
                        break;
                    }
                    Some(record) if record.phase.is_terminal() => break,
                    Some(_) => {}
                    None => break,
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        })
    };
    let outcome = launcher.launch(request);
    drop(launch_guard);
    if let Ok(worker) = &outcome {
        bind_launched_worker(state, repo, &delegation_id, worker, now)?;
        if launch_is_acknowledgement(worker) {
            let record = load(state, repo, &delegation_id).unwrap_or(launched);
            return Ok((record, None));
        }
    }
    watcher_done.store(true, std::sync::atomic::Ordering::Release);
    let _ = watcher.join();
    let cancelled = super::provider::adapter::Cancellation::is_cancelled(cancellation.as_ref());
    let (phase, exit_code, summary) = match &outcome {
        Ok(worker) => (
            if cancelled {
                Phase::Cancelled
            } else if worker.exit_code == 0 {
                Phase::Completed
            } else {
                Phase::Failed
            },
            Some(worker.exit_code),
            worker.receipt.clone(),
        ),
        Err(error) => (Phase::Failed, None, Some(error.to_string())),
    };
    let publication = publish_terminal(
        state,
        repo,
        cfg,
        &delegation_id,
        phase,
        exit_code,
        summary,
        None,
        now,
    )?;
    let record = load(state, repo, &delegation_id).unwrap_or(launched);
    Ok((record, Some(publication)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(delegation: &str, runtime: RuntimeKind) -> WorkerHandle {
        WorkerHandle {
            delegation: delegation.to_string(),
            attempt: 1,
            runtime,
            worker_session: format!("{delegation}-session"),
            short: format!("{delegation}short"),
            role: "worker".to_string(),
            task: Some("task-1".to_string()),
            group: None,
            objective: None,
            workdir: PathBuf::from("."),
            manifest: None,
            plan_override: false,
        }
    }

    fn fixture() -> (tempfile::TempDir, StateDir, PathBuf, CtxConfig) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        (dir, state, repo, CtxConfig::default())
    }

    #[test]
    fn a_launch_receipt_is_durable_before_anything_runs() {
        let (_dir, state, repo, _cfg) = fixture();
        let record = record_launch(
            &state,
            &repo,
            handle("deleg1", RuntimeKind::Native),
            Some("parent".to_string()),
            10,
        )
        .expect("launch receipt");
        assert_eq!(record.phase, Phase::Launched);
        let reread = load(&state, &repo, "deleg1").expect("durable");
        assert_eq!(reread, record);
        assert_eq!(reread.attempts.len(), 1);
    }

    #[test]
    fn a_terminal_outcome_publishes_once_and_replays_as_a_duplicate() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg2", RuntimeKind::Native),
            Some("parent".to_string()),
            10,
        )
        .expect("launch");
        let first = publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg2",
            Phase::Completed,
            Some(0),
            Some("done".to_string()),
            None,
            20,
        )
        .expect("publish");
        assert!(first.published);
        let second = publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg2",
            Phase::Completed,
            Some(0),
            Some("done".to_string()),
            None,
            21,
        )
        .expect("replay");
        assert_eq!(second.identity, first.identity);
        assert!(!second.published, "a replay must not publish twice");
        let record = load(&state, &repo, "deleg2").expect("record");
        assert_eq!(record.published, vec![first.identity.clone()]);
        assert_eq!(record.revision, 1, "a replay must not bump the revision");
    }

    #[test]
    fn duplicate_transport_delivery_is_idempotent_at_the_consumer() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg3", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        let publication = publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg3",
            Phase::Failed,
            Some(1),
            None,
            None,
            2,
        )
        .expect("publish");
        assert!(consume_delivery(&state, &repo, "deleg3", &publication.identity).expect("consume"));
        assert!(
            !consume_delivery(&state, &repo, "deleg3", &publication.identity).expect("replay"),
            "the same delivery identity must be consumed only once"
        );
    }

    #[test]
    fn a_later_revision_is_a_new_delivery_not_a_duplicate() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg4", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        let first = publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg4",
            Phase::Completed,
            Some(0),
            None,
            None,
            2,
        )
        .expect("first");
        let corrected = publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg4",
            Phase::Failed,
            Some(2),
            Some("contract failed".to_string()),
            None,
            3,
        )
        .expect("corrected");
        assert_ne!(first.identity, corrected.identity);
        assert!(corrected.published);
        assert!(consume_delivery(&state, &repo, "deleg4", &corrected.identity).expect("consume"));
    }

    #[test]
    fn a_crash_between_persistence_and_delivery_republishes_the_same_outcome() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg5", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        // Model the crash: the outcome is durable, but `published` never got
        // the identity because the process died before the mail step.
        let mut record = load(&state, &repo, "deleg5").expect("record");
        record.phase = Phase::Completed;
        record.exit_code = Some(0);
        record.revision = 1;
        save(&state, &repo, &record).expect("save");

        let recovered = publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg5",
            Phase::Completed,
            Some(0),
            None,
            None,
            5,
        )
        .expect("recover");
        assert!(recovered.published, "an undelivered outcome must be sent");
        assert_eq!(
            load(&state, &repo, "deleg5").expect("record").revision,
            1,
            "recovery must not invent a second outcome"
        );
    }

    #[test]
    fn a_message_blocked_by_an_open_approval_is_queued_then_delivered_once() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg6", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        let short = "deleg6short";
        super::super::attention::record(
            &state,
            short,
            super::super::attention::Observation::new(
                super::super::attention::Authority::AdapterHook,
                "permission prompt open",
                90,
                1,
            )
            .with_attention(super::super::attention::Attention::Approval),
            1,
        );

        let queued = send(&state, &repo, &cfg, "deleg6", "please continue", 2).expect("send");
        let Dispatch::Queued { reason, .. } = &queued else {
            panic!("an open approval must queue, never type at the dialog: {queued:?}");
        };
        assert_eq!(reason, "approval-open");
        assert!(
            drain_queued(&state, &repo, &cfg, "deleg6", 3)
                .expect("drain")
                .iter()
                .all(|d| matches!(d, Dispatch::Queued { .. })),
            "nothing may be delivered while the latch is still open"
        );

        super::super::attention::record(
            &state,
            short,
            super::super::attention::Observation::new(
                super::super::attention::Authority::AdapterHook,
                "prompt resolved",
                90,
                4,
            )
            .with_attention(super::super::attention::Attention::None),
            4,
        );

        let drained = drain_queued(&state, &repo, &cfg, "deleg6", 5).expect("drain");
        assert_eq!(drained.len(), 1);
        assert!(matches!(drained[0], Dispatch::Delivered { .. }));
        assert!(
            drain_queued(&state, &repo, &cfg, "deleg6", 6)
                .expect("drain again")
                .is_empty(),
            "a drained message must not be delivered a second time"
        );
    }

    #[test]
    fn follow_up_targets_the_original_delegation_and_never_a_recent_session() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg7", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        record_launch(
            &state,
            &repo,
            handle("deleg8", RuntimeKind::Native),
            None,
            9,
        )
        .expect("launch");
        publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg7",
            Phase::Completed,
            Some(0),
            None,
            None,
            2,
        )
        .expect("publish");

        let continuation =
            follow_up(&state, &repo, &cfg, "deleg7", "one more thing", 3).expect("follow up");
        let Continuation::Resume {
            journal_session,
            attempt,
        } = continuation
        else {
            panic!("a finished native worker resumes its own journal: {continuation:?}");
        };
        assert_eq!(journal_session, "deleg7-session");
        assert_eq!(attempt, 2);

        let record = load(&state, &repo, "deleg7").expect("record");
        assert_eq!(record.attempts.len(), 2);
        assert_eq!(
            record.attempts[0].phase,
            Phase::Completed,
            "the prior attempt's outcome must be preserved"
        );

        assert!(
            follow_up(&state, &repo, &cfg, "no-such-delegation", "hi", 4).is_err(),
            "an unknown delegation must be an error, never a recent-session fallback"
        );
    }

    #[test]
    fn a_finished_legacy_worker_gets_a_transparent_checkpoint() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("deleg9", RuntimeKind::Harness),
            None,
            1,
        )
        .expect("launch");
        publish_terminal(
            &state,
            &repo,
            &cfg,
            "deleg9",
            Phase::Completed,
            Some(0),
            None,
            None,
            2,
        )
        .expect("publish");
        let Continuation::Checkpoint { handoff } =
            follow_up(&state, &repo, &cfg, "deleg9", "more", 3).expect("follow up")
        else {
            panic!("no verified resume path means a checkpoint");
        };
        assert!(handoff.contains("REPLACEMENT"));
        assert!(handoff.contains("more"));
    }

    #[test]
    fn close_releases_ownership_but_preserves_receipts_and_unknown_outcomes() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("delega", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        let reservation = super::super::reservation::reserve_within(
            &state,
            "anthropic",
            "delega-session",
            100,
            None,
            1,
        )
        .expect("ledger")
        .expect("reserved");
        record_ownership(
            &state,
            &repo,
            "delega",
            Some(("anthropic".to_string(), reservation.id.clone())),
            Some(repo.clone()),
            1,
        )
        .expect("ownership");
        let mut record = load(&state, &repo, "delega").expect("record");
        record
            .unknown_tool_outcomes
            .push("exec-7: process_start outcome unknown".to_string());
        save(&state, &repo, &record).expect("save");
        let publication = publish_terminal(
            &state,
            &repo,
            &cfg,
            "delega",
            Phase::Cancelled,
            Some(130),
            None,
            None,
            2,
        )
        .expect("publish");

        let closed = close(&state, &repo, "delega", 3).expect("close");
        assert_eq!(closed.phase, Phase::Closed);
        assert!(closed.reservation.is_none());
        assert!(closed.write_claim.is_none());
        assert_eq!(closed.published, vec![publication.identity]);
        assert_eq!(closed.unknown_tool_outcomes.len(), 1);
        assert_eq!(
            super::super::reservation::entries(&state, "anthropic").len(),
            0,
            "close releases the provider reservation"
        );
    }

    #[test]
    fn interrupt_never_erases_an_unknown_effect() {
        let (_dir, state, repo, _cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("delegb", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        let mut record = load(&state, &repo, "delegb").expect("record");
        record
            .unknown_tool_outcomes
            .push("exec-1: apply_patch outcome unknown".to_string());
        save(&state, &repo, &record).expect("save");
        let after = interrupt(&state, &repo, "delegb", 5).expect("interrupt");
        assert_eq!(after.phase, Phase::Launched);
        assert!(after.cancel_requested);
        assert_eq!(after.unknown_tool_outcomes.len(), 1);
    }

    #[test]
    fn interrupt_stops_the_addressed_worker_before_close_releases_resources() {
        let (_dir, state, repo, _cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("issue573", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        record_ownership(
            &state,
            &repo,
            "issue573",
            Some(("provider".to_string(), "reservation".to_string())),
            Some(repo.clone()),
            2,
        )
        .expect("ownership");

        let interrupted = interrupt(&state, &repo, "issue573", 3).expect("interrupt");
        assert!(interrupted.cancel_requested);
        assert_eq!(interrupted.phase, Phase::Launched);
        assert!(
            close(&state, &repo, "issue573", 4).is_err(),
            "close must wait for the worker's terminal acknowledgement"
        );
        let held = load(&state, &repo, "issue573").expect("record");
        assert!(held.reservation.is_some());
        assert!(held.write_claim.is_some());

        #[derive(Debug)]
        struct CancelAwareLauncher(std::sync::Arc<std::sync::Mutex<Vec<RuntimeKind>>>);

        impl WorkerLauncher for CancelAwareLauncher {
            fn launch(&mut self, request: &LaunchRequest) -> CtxResult<LaunchedWorker> {
                use super::super::provider::adapter::Cancellation as _;
                while !request.cancellation.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                self.0.lock().expect("seen lock").push(request.runtime);
                Ok(LaunchedWorker {
                    exit_code: 130,
                    session: request.worker_session.clone().unwrap_or_default(),
                    short: request
                        .worker_session
                        .as_deref()
                        .map(super::super::sessions::short_id)
                        .unwrap_or_default(),
                    receipt: Some("{\"state\":\"exited_no_report\"}".to_string()),
                })
            }
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        for runtime in [RuntimeKind::Native, RuntimeKind::Harness] {
            let state_worker = state.clone();
            let repo_worker = repo.clone();
            let cfg = CtxConfig::default();
            let seen_worker = seen.clone();
            let mut request = launch_request(super::super::team::IMPLEMENTER, false);
            request.runtime = runtime;
            request.task = Some(format!("cancel-{runtime}"));
            let task = request.task.clone();
            let worker = std::thread::spawn(move || {
                delegate(
                    &state_worker,
                    &repo_worker,
                    &cfg,
                    &mut CancelAwareLauncher(seen_worker),
                    &request,
                    &coordinator_parent(),
                    10,
                )
                .map_err(|error| error.to_string())
            });

            let delegation = (0..100)
                .find_map(|_| {
                    let found = list(&state, &repo)
                        .into_iter()
                        .find(|record| record.handle.task == task);
                    if found.is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    found.map(|record| record.handle.delegation)
                })
                .expect("launch receipt");
            interrupt(&state, &repo, &delegation, 11).expect("interrupt live worker");
            let (terminal, _) = worker
                .join()
                .expect("worker joins")
                .expect("delegate returns");
            assert_eq!(terminal.phase, Phase::Cancelled);
            close(&state, &repo, &delegation, 12).expect("terminal worker closes");
        }
        let observed = seen.lock().expect("seen lock");
        assert_eq!(observed.len(), 2);
        assert!(observed.contains(&RuntimeKind::Harness));
        assert!(observed.contains(&RuntimeKind::Native));
    }

    #[test]
    fn a_bounded_wait_answers_from_durable_state_alone() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("delegc", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        assert_eq!(
            wait(&state, &repo, "delegc", 2, 10).expect("wait"),
            WaitOutcome::Pending
        );
        assert_eq!(
            wait(&state, &repo, "delegc", 11, 10).expect("wait"),
            WaitOutcome::TimedOut
        );
        publish_terminal(
            &state,
            &repo,
            &cfg,
            "delegc",
            Phase::Completed,
            Some(0),
            None,
            None,
            12,
        )
        .expect("publish");
        assert!(matches!(
            wait(&state, &repo, "delegc", 13, 10).expect("wait"),
            WaitOutcome::Ready(_)
        ));
    }

    #[test]
    fn a_result_manifest_is_bounded_and_says_what_it_cut() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("delegd", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");
        publish_terminal(
            &state,
            &repo,
            &cfg,
            "delegd",
            Phase::Completed,
            Some(0),
            Some("x".repeat(500)),
            Some(PathBuf::from("report.md")),
            2,
        )
        .expect("publish");
        let manifest = result(&state, &repo, "delegd", 64).expect("manifest");
        assert!(manifest.summary_truncated);
        assert_eq!(manifest.summary.as_deref().map(str::len), Some(64));
        assert_eq!(manifest.deliveries.len(), 1);
        assert_eq!(manifest.continuation, "resume");
    }

    #[test]
    fn a_delegation_id_from_untrusted_output_cannot_escape_its_directory() {
        let (_dir, state, repo, _cfg) = fixture();
        for bad in ["../escape", "a/b", "", "with space"] {
            assert!(
                record_launch(&state, &repo, handle(bad, RuntimeKind::Native), None, 1).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    /// Review finding on `publish_terminal` (~422-425): the identity used to
    /// be pushed into `record.published` even when `notify_parent` returned
    /// `Err`, which broke the `Publication::mailed` doc's own promise that "a
    /// later sweep can retry it" -- nothing ever retried, because the
    /// identity already looked published. Blocks the parent's mailbox with a
    /// plain file (so `create_private_dir_all` fails deterministically),
    /// publishes, confirms the failure left the identity retryable, unblocks
    /// the mailbox, and drives `drain_all` -- the same sweep `mail.rs`'s
    /// inbox rendering already calls on every checkpoint -- to prove the
    /// outcome is delivered exactly once.
    #[test]
    fn a_mail_transport_that_fails_once_then_succeeds_delivers_the_outcome_exactly_once() {
        let (_dir, state, repo, cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("delege", RuntimeKind::Native),
            Some("parent".to_string()),
            1,
        )
        .expect("launch");

        let slug = repo_slug(&repo);
        std::fs::create_dir_all(state.mail()).expect("mail root");
        let mailbox = state.mail().join(&slug);
        std::fs::write(&mailbox, b"blocker").expect("block the mailbox with a plain file");

        let first = publish_terminal(
            &state,
            &repo,
            &cfg,
            "delege",
            Phase::Completed,
            Some(0),
            Some("done".to_string()),
            None,
            2,
        )
        .expect("the terminal outcome is durable even when mail fails");
        assert!(first.published);
        assert!(!first.mailed, "the transport failure must be visible");
        let record = load(&state, &repo, "delege").expect("record");
        assert!(
            record.published.is_empty(),
            "a failed mail must NOT be marked published, or the sweep below has nothing to \
             retry"
        );

        std::fs::remove_file(&mailbox).expect("unblock the mailbox");
        let delivered = drain_all(&state, &repo, &cfg, 3);
        assert_eq!(delivered, 1, "the sweep must retry the failed publication");

        let record = load(&state, &repo, "delege").expect("record");
        assert_eq!(record.published, vec![first.identity.clone()]);

        let messages: Vec<_> = std::fs::read_dir(&mailbox)
            .expect("mailbox dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
            .collect();
        assert_eq!(messages.len(), 1, "exactly one delivery, never a duplicate");

        assert_eq!(
            drain_all(&state, &repo, &cfg, 4),
            0,
            "a second sweep must not re-deliver an outcome already published"
        );
    }

    /// Review finding on `save`/`load` (~240-254): plain file I/O with no
    /// lock, unlike `task.rs`'s `lock_tasks`/`with_task_lock`, so two
    /// concurrent mutators of the SAME record could lose one's update to the
    /// other's stale-read overwrite. Mirrors `group.rs`'s own `group_
    /// mutations_wait_for_the_same_interprocess_lock` test: holds the
    /// delegation lock externally, confirms BOTH of two independent mutators
    /// (`interrupt` and `record_ownership`, which touch disjoint fields) wait
    /// rather than racing ahead, releases it, and asserts both of their
    /// changes survived -- neither was clobbered by the other's write.
    #[test]
    fn two_concurrent_mutators_of_the_same_delegation_record_both_persist() {
        let (_dir, state, repo, _cfg) = fixture();
        record_launch(
            &state,
            &repo,
            handle("delege", RuntimeKind::Native),
            None,
            1,
        )
        .expect("launch");

        let held = lock_delegation(&state, &repo, "delege").expect("hold delegation lock");

        let (start_a_tx, start_a_rx) = std::sync::mpsc::sync_channel(0);
        let (done_a_tx, done_a_rx) = std::sync::mpsc::sync_channel(0);
        let state_a = state.clone();
        let repo_a = repo.clone();
        let worker_a = std::thread::spawn(move || {
            start_a_tx.send(()).expect("announce a started");
            let result = interrupt(&state_a, &repo_a, "delege", 5).map(|_| ());
            done_a_tx
                .send(result.map_err(|e| e.to_string()))
                .expect("announce a done");
        });

        let (start_b_tx, start_b_rx) = std::sync::mpsc::sync_channel(0);
        let (done_b_tx, done_b_rx) = std::sync::mpsc::sync_channel(0);
        let state_b = state.clone();
        let repo_b = repo.clone();
        let worker_b = std::thread::spawn(move || {
            start_b_tx.send(()).expect("announce b started");
            let result = record_ownership(
                &state_b,
                &repo_b,
                "delege",
                Some(("anthropic".to_string(), "res-1".to_string())),
                Some(repo_b.clone()),
                6,
            );
            done_b_tx
                .send(result.map_err(|e| e.to_string()))
                .expect("announce b done");
        });

        start_a_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("a started");
        start_b_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("b started");
        assert!(
            done_a_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
                && done_b_rx
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .is_err(),
            "both mutators must wait while another holder owns the delegation lock"
        );

        drop(held);
        done_a_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a finished")
            .expect("a succeeded");
        done_b_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("b finished")
            .expect("b succeeded");
        worker_a.join().expect("a joins");
        worker_b.join().expect("b joins");

        let record = load(&state, &repo, "delege").expect("record");
        assert!(
            record.cancel_requested,
            "interrupt's own change must have persisted"
        );
        assert_eq!(
            record.reservation,
            Some(("anthropic".to_string(), "res-1".to_string())),
            "record_ownership's own change must have persisted too -- neither mutator's write \
             may be lost to the other's stale-read overwrite"
        );
    }

    // -- the launch seam's bounds (issue #485, roadmap N16) ---------------

    fn launch_request(role: &str, read_only: bool) -> LaunchRequest {
        LaunchRequest {
            runtime: RuntimeKind::Native,
            target: "fast".to_string(),
            brief: "do the thing".to_string(),
            role: role.to_string(),
            task: Some(format!("task-{role}")),
            group: None,
            workdir: None,
            manifest: None,
            plan_override_requested: false,
            manifest_registry: None,
            read_only,
            budget_tokens: None,
            max_tool_calls: None,
            worker_session: None,
            delegated_depth: None,
            parent_envelope: None,
            parent_principal: None,
            cancellation: std::sync::Arc::new(
                super::super::provider::adapter::CancellationFlag::default(),
            ),
        }
    }

    fn coordinator_parent() -> Parent<'static> {
        Parent {
            session: Some("coord-session"),
            short: "coord001",
            role: super::super::team::COORDINATOR,
            depth: 2,
            generation: None,
            generation_locked: false,
        }
    }

    /// Issue #541 chunk C follow-up: `delegate` itself never reads a
    /// filesystem or a home directory, but when the CALLER resolves an
    /// operator-global (or repository) manifest and hands the registry in
    /// through `LaunchRequest.manifest_registry`, that manifest is admitted
    /// exactly like a built-in one -- the gap that made a real coordinator
    /// unable to delegate with anything but the twelve built-ins.
    #[test]
    fn a_delegation_with_an_operator_manifest_is_admitted_when_the_caller_resolves_it() {
        let (_dir, state, repo, cfg) = fixture();
        let home = tempfile::tempdir().expect("home");
        let global = home.path().join(".zirv").join("agents");
        std::fs::create_dir_all(&global).expect("agents dir");
        std::fs::write(
            global.join("custom-implementer.yaml"),
            "schema_version: 1\nid: custom-implementer\nversion: 1\nname: Custom\n\
             description: operator custom implementer\nrole: custom\nmodel_tier: standard\n\
             read_only: false\nrequired_capabilities: [repo.read, repo.write]\n\
             context_budget_bytes: 64\ninstructions: implement only the assigned scope\n\
             team_role: implementer\n",
        )
        .expect("write manifest");
        let registry = crate::commands::workflow::agents::AgentRegistry::load(
            &repo,
            Some(home.path()),
            true,
            false,
        )
        .expect("registry");

        let mut request = launch_request(super::super::team::IMPLEMENTER, false);
        request.manifest = Some("custom-implementer".to_string());
        request.manifest_registry = Some(std::sync::Arc::new(registry));

        let mut launcher = RecordingLauncher::default();
        let (record, _publication) = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &request,
            &coordinator_parent(),
            10,
        )
        .expect("an operator manifest the caller resolved is admitted");
        assert_eq!(
            record.handle.manifest.as_deref(),
            Some("custom-implementer")
        );
    }

    /// The other direction: a caller that resolves NOTHING (`manifest_
    /// registry: None`, the same as every pre-existing caller) still gets
    /// the built-in-only refusal for an id that is not a built-in either --
    /// the fallback is a narrower lookup, never a bypass of the check
    /// itself.
    #[test]
    fn an_unknown_manifest_is_still_refused_when_the_caller_resolves_nothing() {
        let (_dir, state, repo, cfg) = fixture();
        let mut request = launch_request(super::super::team::IMPLEMENTER, false);
        request.manifest = Some("does-not-exist".to_string());
        request.manifest_registry = None;

        let mut launcher = RecordingLauncher::default();
        let error = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &request,
            &coordinator_parent(),
            10,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown manifest"), "{error}");
        assert!(launcher.launches.lock().expect("lock").is_empty());
    }

    /// A REAL compiled bug-fix plan: `debugger-1` then `implementer-1`
    /// (`depends_on: ["debugger-1"]`), both claiming `["primary"]` -- the
    /// exact shape `team::compile` gives a low-risk bug fix, and the shape
    /// review finding 1 (issue #541 chunk C) showed could never actually
    /// dispatch its second seat.
    fn bugfix_plan(repo: &Path) -> crate::commands::workflow::team::TeamPlan {
        use crate::commands::workflow::agents::AgentRegistry;
        use crate::commands::workflow::classify::{
            Classification, Complexity, DomainClassification, Intent, RiskBand, RiskMeasurement,
        };
        use crate::commands::workflow::profile::ExecutionProfile;
        use crate::commands::workflow::skill::SkillRegistry;
        use crate::commands::workflow::team;

        let classification = Classification {
            intent: Intent::Bugfix,
            complexity: Complexity::Bounded,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 2,
            changed_lines: 20,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::Measured,
            reasons: vec!["test fixture".to_string()],
        };
        let profile = ExecutionProfile::derive("fix the null pointer crash", &classification);
        let registry = AgentRegistry::load(repo, None, false, false).expect("registry");
        let skills = SkillRegistry::load(repo, None, false, false).expect("skills");
        let plan = team::compile(
            "fix the null pointer crash",
            &profile,
            &registry,
            &skills,
            &|_role| Ok(()),
        )
        .expect("bugfix plan compiles");
        assert_eq!(
            plan.seats.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["debugger-1", "implementer-1"],
            "fixture drifted from team::compile's own bugfix shape: {plan:?}"
        );
        plan
    }

    /// Issue #541 chunk C review finding 1, half 1 (settlement releases a
    /// claim): `debugger-1` and `implementer-1` share a claim on purpose --
    /// a sequential hand-off, not a genuine overlap -- so once the debugger
    /// SETTLES, the dependent implementer is admitted rather than refused
    /// with `ClaimConflict` forever.
    #[test]
    fn a_settled_debuggers_claim_is_released_so_the_dependent_implementer_is_admitted() {
        let (_dir, state, repo, cfg) = fixture();
        let plan = bugfix_plan(&repo);
        super::super::coordinator::update(&state, &repo, |graph| {
            graph.store_team_plan_inline(plan, 1);
        })
        .expect("store plan");

        let mut debugger_request = launch_request(super::super::team::IMPLEMENTER, false);
        debugger_request.manifest = Some("debugger".to_string());
        debugger_request.task = Some("debugger-1".to_string());
        let mut launcher = RecordingLauncher::default();
        let (debugger_record, _publication) = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &debugger_request,
            &coordinator_parent(),
            10,
        )
        .expect("debugger-1 dispatched");

        // Settle it: publish the terminal outcome and let the coordinator
        // consume the receipt, exactly like a real restart/poll would.
        publish_terminal(
            &state,
            &repo,
            &cfg,
            &debugger_record.handle.delegation,
            Phase::Completed,
            Some(0),
            Some("root cause found".to_string()),
            None,
            20,
        )
        .expect("publish debugger outcome");
        let mut graph = super::super::coordinator::load(&state, &repo);
        super::super::coordinator::consume_pending(&state, &repo, &mut graph, 21)
            .expect("consume debugger receipt");
        assert!(
            !graph.seat_claim_active("debugger-1"),
            "a completed seat's claim must be released"
        );

        // The dependent implementer -- SAME claim, `depends_on: [debugger-1]`
        // -- is now admitted, not refused with ClaimConflict.
        let mut implementer_request = launch_request(super::super::team::IMPLEMENTER, false);
        implementer_request.manifest = Some("implementer".to_string());
        implementer_request.task = Some("implementer-1".to_string());
        let (implementer_record, _publication) = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &implementer_request,
            &coordinator_parent(),
            22,
        )
        .expect("implementer-1 admitted once the debugger has settled");
        assert_eq!(
            implementer_record.handle.manifest.as_deref(),
            Some("implementer")
        );
    }

    /// Issue #541 chunk C review finding 1, half 2 (ancestor exclusion) plus
    /// the negative case: two seats with NO dependency relationship that
    /// genuinely claim the same paths are still refused, even while the
    /// first is still in flight -- the fix narrows the conflict rule, it
    /// does not disable it.
    #[test]
    fn two_independent_writers_with_overlapping_claims_are_still_refused() {
        use crate::commands::workflow::agents::AgentRegistry;
        use crate::commands::workflow::classify::{
            Classification, Complexity, DomainClassification, Intent, RiskBand, RiskMeasurement,
        };
        use crate::commands::workflow::profile::ExecutionProfile;
        use crate::commands::workflow::skill::SkillRegistry;
        use crate::commands::workflow::team;

        let (_dir, state, repo, cfg) = fixture();
        let classification = Classification {
            intent: Intent::Feature,
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 5,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::Measured,
            reasons: vec!["test fixture".to_string()],
        };
        let profile = ExecutionProfile::derive("two independent writers", &classification);
        let registry = AgentRegistry::load(&repo, None, false, false).expect("registry");
        let skills = SkillRegistry::load(&repo, None, false, false).expect("skills");
        let always_eligible = |_role: super::super::team::TeamRole| Ok(());
        let mut plan = team::compile_explicit(
            "implement A",
            &profile,
            &registry,
            &skills,
            &always_eligible,
            "implementer",
        )
        .expect("seat a compiles");
        let mut seat_a = plan.seats.remove(0);
        seat_a.id = "implementer-a".to_string();
        seat_a.claim.paths = vec!["src/shared.rs".to_string()];
        let plan_b = team::compile_explicit(
            "implement B",
            &profile,
            &registry,
            &skills,
            &always_eligible,
            "implementer",
        )
        .expect("seat b compiles");
        let mut seat_b = plan_b.seats.into_iter().next().expect("seat b");
        seat_b.id = "implementer-b".to_string();
        // Genuinely overlapping, and deliberately NOT an ancestor of `seat_a`
        // (no `depends_on` either way): the shape the conflict rule must
        // still catch.
        seat_b.claim.paths = vec!["src/shared.rs".to_string()];
        plan.seats = vec![seat_a, seat_b];
        super::super::coordinator::update(&state, &repo, |graph| {
            graph.store_team_plan_inline(plan, 1);
        })
        .expect("store plan");

        let mut request_a = launch_request(super::super::team::IMPLEMENTER, false);
        request_a.manifest = Some("implementer".to_string());
        request_a.task = Some("implementer-a".to_string());
        let mut launcher = RecordingLauncher::default();
        delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &request_a,
            &coordinator_parent(),
            10,
        )
        .expect("the first independent writer is admitted");

        let mut request_b = launch_request(super::super::team::IMPLEMENTER, false);
        request_b.manifest = Some("implementer".to_string());
        request_b.task = Some("implementer-b".to_string());
        let error = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &request_b,
            &coordinator_parent(),
            11,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("claims paths another currently-dispatched seat"),
            "{error}"
        );
    }

    /// Issue #485 item 3: a delegation that cannot be admitted leaves NO
    /// launch receipt -- a durable record naming work nobody started is
    /// exactly the confusion the receipt exists to prevent -- and the
    /// launcher is never reached.
    #[test]
    fn a_refused_delegation_starts_nothing_and_writes_no_receipt() {
        let (_dir, state, repo, cfg) = fixture();
        let mut launcher = RecordingLauncher::default();
        let launches = launcher.launches.clone();
        let parent = Parent {
            role: super::super::team::REVIEWER,
            ..coordinator_parent()
        };
        let error = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &parent,
            10,
        )
        .expect_err("a reviewer seat may not delegate");
        assert!(error.to_string().contains("may not delegate"), "{error}");
        assert!(launches.lock().expect("lock").is_empty());
        assert!(list(&state, &repo).is_empty(), "no receipt was written");

        // Depth is the other identity-decidable bound, and it refuses the
        // same way.
        let exhausted = Parent {
            depth: 0,
            ..coordinator_parent()
        };
        assert!(
            delegate(
                &state,
                &repo,
                &cfg,
                &mut launcher,
                &launch_request(super::super::team::IMPLEMENTER, false),
                &exhausted,
                10,
            )
            .is_err()
        );
        assert!(list(&state, &repo).is_empty());
    }

    /// Issue #485 item 6, at the seam that actually launches: the child's
    /// mode is its OWN role's, in both directions.
    #[test]
    fn the_childs_mode_is_decided_by_its_role_not_by_the_request() {
        let (_dir, state, repo, cfg) = fixture();
        let mut launcher = RecordingLauncher::default();
        let launches = launcher.launches.clone();

        // A reviewer asked for as a writer is launched read-only.
        delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::REVIEWER, false),
            &coordinator_parent(),
            10,
        )
        .expect("admitted");
        // An implementer is launched writing, and nothing about the
        // delegating seat's own posture changes that.
        delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            11,
        )
        .expect("admitted");

        let seen = launches.lock().expect("lock");
        assert_eq!(seen.len(), 2);
        assert!(seen[0].read_only, "a reviewer is never handed a checkout");
        assert!(!seen[1].read_only, "an implementer is");
    }

    /// Issue #485 item 4: the coordinator's graph names the delegation that
    /// is answering for each task, written beside the launch receipt rather
    /// than reconstructed from a transcript later.
    #[test]
    fn the_launch_binds_the_task_to_its_delegation_in_the_coordinators_graph() {
        let (_dir, state, repo, cfg) = fixture();
        let mut launcher = RecordingLauncher::default();
        let (record, _) = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            10,
        )
        .expect("admitted");

        let graph = super::super::coordinator::load(&state, &repo);
        let node = graph.nodes.get("task-implementer").expect("node");
        assert_eq!(
            node.delegation.as_deref(),
            Some(record.handle.delegation.as_str())
        );
        assert_eq!(node.runtime.as_deref(), Some("native"));
        assert_eq!(node.role, super::super::team::IMPLEMENTER);
        assert_eq!(
            node.state,
            super::super::coordinator::NodeState::Delegated,
            "the node stays delegated until its receipt is CONSUMED, not merely published"
        );
    }

    #[test]
    fn delegation_handle_resumes_the_launched_worker_conversation() {
        #[derive(Debug)]
        struct ConversationLauncher;

        impl WorkerLauncher for ConversationLauncher {
            fn launch(&mut self, _request: &LaunchRequest) -> CtxResult<LaunchedWorker> {
                Ok(LaunchedWorker {
                    exit_code: 0,
                    session: "actual-worker-conversation".to_string(),
                    short: "actual01".to_string(),
                    receipt: Some("{\"state\":\"reported\"}".to_string()),
                })
            }
        }

        let (_dir, state, repo, cfg) = fixture();
        let (record, _) = delegate(
            &state,
            &repo,
            &cfg,
            &mut ConversationLauncher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            10,
        )
        .expect("delegate");
        assert_eq!(record.handle.worker_session, "actual-worker-conversation");
        assert_eq!(record.handle.short, "actual01");
        assert_eq!(
            follow_up(
                &state,
                &repo,
                &cfg,
                &record.handle.delegation,
                "continue",
                11
            )
            .expect("follow up"),
            Continuation::Resume {
                journal_session: "actual-worker-conversation".to_string(),
                attempt: 2,
            }
        );
    }

    #[test]
    fn dashboard_spawn_ack_does_not_complete_delegation() {
        #[derive(Debug)]
        struct DashboardAckLauncher;

        impl WorkerLauncher for DashboardAckLauncher {
            fn launch(&mut self, _request: &LaunchRequest) -> CtxResult<LaunchedWorker> {
                Ok(LaunchedWorker {
                    exit_code: 0,
                    session: "dashboard-worker".to_string(),
                    short: "dash0001".to_string(),
                    receipt: Some("{\"state\":\"launched\"}".to_string()),
                })
            }
        }

        let (_dir, state, repo, cfg) = fixture();
        let (running, publication) = delegate(
            &state,
            &repo,
            &cfg,
            &mut DashboardAckLauncher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            10,
        )
        .expect("delegate");
        assert_eq!(running.phase, Phase::Launched);
        assert!(publication.is_none());

        publish_terminal(
            &state,
            &repo,
            &cfg,
            &running.handle.delegation,
            Phase::Completed,
            Some(0),
            Some("worker receipt".to_string()),
            None,
            11,
        )
        .expect("terminal worker receipt");
        assert_eq!(
            load(&state, &repo, &running.handle.delegation)
                .expect("record")
                .phase,
            Phase::Completed
        );
    }

    #[test]
    fn interrupt_after_dashboard_launch_ack_cancels_the_worker() {
        #[derive(Debug)]
        struct DashboardAckLauncher {
            cancellation: std::sync::Arc<
                std::sync::Mutex<
                    Option<std::sync::Arc<super::super::provider::adapter::CancellationFlag>>,
                >,
            >,
        }

        impl WorkerLauncher for DashboardAckLauncher {
            fn launch(&mut self, request: &LaunchRequest) -> CtxResult<LaunchedWorker> {
                *self.cancellation.lock().expect("cancellation") =
                    Some(request.cancellation.clone());
                Ok(LaunchedWorker {
                    exit_code: 0,
                    session: "dashboard-worker".to_string(),
                    short: "dash0001".to_string(),
                    receipt: Some("{\"state\":\"launched\"}".to_string()),
                })
            }
        }

        let (_dir, state, repo, cfg) = fixture();
        let cancellation = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut launcher = DashboardAckLauncher {
            cancellation: cancellation.clone(),
        };
        let (running, _) = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            10,
        )
        .expect("delegate");

        interrupt(&state, &repo, &running.handle.delegation, 11).expect("interrupt");
        let flag = cancellation
            .lock()
            .expect("cancellation")
            .clone()
            .expect("worker cancellation");
        use super::super::provider::adapter::Cancellation as _;
        for _ in 0..100 {
            if flag.is_cancelled() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(flag.is_cancelled(), "the live worker was not cancelled");
        assert_eq!(
            load(&state, &repo, &running.handle.delegation)
                .expect("record")
                .phase,
            Phase::Launched
        );
    }

    /// Review finding on issue #485: the launch receipt is authoritative, so
    /// a coordinator-graph store failure must never block it -- but it must
    /// not vanish silently either. Forces `coordinator::store` to fail by
    /// occupying its directory path with a plain FILE (a portable failure:
    /// "create a directory where a file already exists" fails identically on
    /// every platform, unlike a Unix permission bit), then asserts the
    /// launch still proceeds AND the decision log gets a line naming it.
    #[test]
    fn a_coordinator_store_failure_never_blocks_the_launch_and_is_logged() {
        let (_dir, state, repo, cfg) = fixture();
        std::fs::create_dir_all(state.root()).expect("state root");
        std::fs::write(state.coordinator(), b"not a directory").expect("occupy the path");
        let mut launcher = RecordingLauncher::default();
        let launches = launcher.launches.clone();

        let (record, _) = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            10,
        )
        .expect("a graph-store failure must not refuse the delegation");
        assert_eq!(
            launches.lock().expect("lock").len(),
            1,
            "the worker was launched despite the graph store failing"
        );
        assert_eq!(record.phase, Phase::Completed);

        let decisions = super::super::log::read_decisions(&state);
        let logged = decisions
            .iter()
            .find(|entry| entry.action == "coordinator-store-failed")
            .expect("the store failure was logged, not swallowed");
        assert_eq!(logged.verb, "delegation");
        assert!(
            logged.detail.contains(&record.handle.delegation),
            "{}",
            logged.detail
        );
    }

    /// Issue #485 item 7: once the user cancels, nothing further is
    /// dispatched -- and the refusal says so rather than failing opaquely.
    #[test]
    fn a_cancelled_objective_admits_no_further_delegations() {
        let (_dir, state, repo, cfg) = fixture();
        let mut graph = super::super::coordinator::Coordinator::default();
        graph.cancel(5);
        super::super::coordinator::store(&state, &repo, &graph).expect("store");

        let mut launcher = RecordingLauncher::default();
        let error = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &coordinator_parent(),
            10,
        )
        .expect_err("cancelled");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(list(&state, &repo).is_empty());
    }

    /// Issue #488 item 4: the delegation service is generation-fenced. A
    /// delegator whose seat a rollover superseded starts nothing and leaves
    /// no launch receipt, and the successor of a rollover that is only
    /// PREPARED is refused for the other reason -- it does not hold the seat
    /// yet.
    #[test]
    fn a_stale_or_uncommitted_generation_may_not_delegate() {
        use super::super::seat;
        let (_dir, state, repo, cfg) = fixture();
        let session = "1c2d3e4f-aaaa-4bbb-8ccc-0123456789ab";
        let short = super::super::sessions::short_id(session);
        seat::register(
            &state,
            &short,
            session,
            "native",
            None,
            "anthropic",
            super::super::team::COORDINATOR,
            false,
            1,
        )
        .expect("register");
        let prepared = seat::prepare_onto(
            &state,
            &short,
            "claude",
            None,
            RuntimeKind::Harness,
            seat::Cause::Manual,
            2,
        )
        .expect("prepare");

        let mut launcher = RecordingLauncher::default();
        let launches = launcher.launches.clone();
        let parent = Parent {
            short: &short,
            generation: Some(prepared),
            ..coordinator_parent()
        };
        let error = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &parent,
            10,
        )
        .expect_err("a successor that has not committed may not delegate");
        assert!(error.to_string().contains("uncommitted"), "{error}");
        assert!(launches.lock().expect("lock").is_empty());
        assert!(list(&state, &repo).is_empty(), "no receipt was written");

        seat::commit(&state, &short, prepared, "successor-session", 3).expect("commit");
        let superseded = Parent {
            short: &short,
            generation: Some(1),
            ..coordinator_parent()
        };
        let error = delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &superseded,
            11,
        )
        .expect_err("the superseded source may not delegate");
        assert!(error.to_string().contains("superseded"), "{error}");
        assert!(list(&state, &repo).is_empty());

        // And the live generation still delegates normally.
        let live = Parent {
            short: &short,
            generation: Some(prepared),
            ..coordinator_parent()
        };
        delegate(
            &state,
            &repo,
            &cfg,
            &mut launcher,
            &launch_request(super::super::team::IMPLEMENTER, false),
            &live,
            12,
        )
        .expect("the committed generation delegates");
        assert_eq!(list(&state, &repo).len(), 1);
    }
}
