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

    let mailed = notify_parent(state, repo, cfg, &record, &identity).is_ok();
    record.published.push(identity.clone());
    record.updated_at = now;
    save(state, repo, &record)?;
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
    let Some(mut record) = load(state, repo, delegation) else {
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
/// only ADDS to `unknown_tool_outcomes` and never clears it.
pub fn interrupt(state: &StateDir, repo: &Path, delegation: &str, now: u64) -> CtxResult<Record> {
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
    record.cancel_requested = true;
    if !record.phase.is_terminal() {
        record.phase = Phase::Cancelled;
        record.revision = record.revision.saturating_add(1);
        if let Some(attempt) = record.attempts.last_mut() {
            attempt.phase = Phase::Cancelled;
            attempt.ended_at = Some(now);
        }
    }
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
    let Some(mut record) = load(state, repo, delegation) else {
        return Err(format!("no delegation {delegation:?} in this repository").into());
    };
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

/// Whether `identity` names a delegation outcome this repository has ALREADY
/// consumed -- i.e. whether the message carrying it is a duplicate transport
/// delivery of something the consumer has already acted on.
///
/// Fails open on purpose: an identity that names no delegation record here
/// (an outcome from another repository, a hand-written line, a record swept
/// away) is reported as new. Hiding a message nobody can account for would
/// turn a bookkeeping gap into lost mail, which is the failure this whole
/// mechanism exists to prevent.
pub fn is_duplicate_delivery(state: &StateDir, repo: &Path, identity: &str) -> bool {
    let Some(delegation) = identity.split(':').next() else {
        return false;
    };
    match consume_delivery(state, repo, delegation, identity) {
        Ok(first_time) => !first_time,
        Err(_) => false,
    }
}

/// Retries every delegation's deferred messages at THIS boundary, and reports
/// how many were actually delivered. Called from the orchestrator-side
/// checkpoint (`zirv ctx inbox`), which is by construction a moment no
/// approval dialog is open on the caller -- the #468 rule, applied per
/// worker rather than per pane.
pub fn drain_all(state: &StateDir, repo: &Path, cfg: &CtxConfig, now: u64) -> usize {
    let mut delivered = 0;
    for record in list(state, repo) {
        if record.queued.is_empty() {
            continue;
        }
        if let Ok(dispatches) = drain_queued(state, repo, cfg, &record.handle.delegation, now) {
            delivered += dispatches
                .iter()
                .filter(|dispatch| matches!(dispatch, Dispatch::Delivered { .. }))
                .count();
        }
    }
    delivered
}

// -- the launch seam -----------------------------------------------------

/// One request to actually START a worker, independent of which runtime will
/// run it. Everything here is data a parent can legitimately ask for; nothing
/// is a grant (`mode`, `path_scope` and the rest are narrowed against the
/// parent's own envelope by `agent::run_with`, never widened by asking).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub runtime: RuntimeKind,
    /// The harness name (harness runtime) or the provider route (native).
    pub target: String,
    pub brief: String,
    pub role: String,
    pub task: Option<String>,
    pub group: Option<String>,
    pub workdir: Option<PathBuf>,
    pub read_only: bool,
    pub budget_tokens: Option<u64>,
    pub max_tool_calls: Option<u32>,
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
            ..Default::default()
        };
        let mut out: Vec<u8> = Vec::new();
        let code = super::agent::run_with(
            &args,
            &mut out,
            &self.repo,
            &super::config::env_from_process(),
        )?;
        let receipt = String::from_utf8_lossy(&out).trim().to_string();
        Ok(LaunchedWorker {
            exit_code: code,
            session: String::new(),
            short: String::new(),
            receipt: (!receipt.is_empty()).then_some(receipt),
        })
    }
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

/// Registers one delegation and starts its worker, in that order.
///
/// The launch receipt is durable BEFORE `launcher` is called, so a crash
/// inside the launch still leaves a record naming the work that may have
/// started -- the opposite order would lose it. The terminal outcome is then
/// published through [`publish_terminal`], which is where the delivery
/// identity a consumer deduplicates on comes from.
/// Eight arguments, over clippy's default seven, for the same reason
/// [`publish_terminal`] documents just above.
#[allow(clippy::too_many_arguments)]
pub fn delegate(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    launcher: &mut dyn WorkerLauncher,
    request: &LaunchRequest,
    parent_session: Option<String>,
    parent_short: &str,
    now: u64,
) -> CtxResult<(Record, Publication)> {
    let delegation_id = uuid::Uuid::new_v4().simple().to_string();
    let worker_session = format!("{delegation_id}-worker");
    let handle = WorkerHandle {
        delegation: delegation_id.clone(),
        attempt: 1,
        runtime: request.runtime,
        worker_session: worker_session.clone(),
        short: parent_short.to_string(),
        role: request.role.clone(),
        task: request.task.clone(),
        group: request.group.clone(),
        objective: None,
        workdir: request
            .workdir
            .clone()
            .unwrap_or_else(|| repo.to_path_buf()),
    };
    let launched = record_launch(state, repo, handle, parent_session, now)?;

    let outcome = launcher.launch(request);
    let (phase, exit_code, summary) = match &outcome {
        Ok(worker) => (
            if worker.exit_code == 0 {
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
    Ok((record, publication))
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
        assert_eq!(after.phase, Phase::Cancelled);
        assert!(after.cancel_requested);
        assert_eq!(after.unknown_tool_outcomes.len(), 1);
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
}
