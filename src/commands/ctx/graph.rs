//! `zirv ctx graph` (issue #832): the agent graph registry and the merged
//! event log, the data layer a later tree view builds on.
//!
//! Write side: claude's `SubagentStart`/`SubagentStop` hooks each make ONE
//! local file write (`<state>/graph/<session short>/<agent_id>.json`); they
//! print nothing, never fail and never touch the network. Node files are
//! pruned with their session (see [`prune`]).
//!
//! Two more small records feed the read side. Every `Agent` dispatch appends
//! `{tool_use_id, caller_agent_id, ts, workflow}` to `<state>/graph-dispatch/<session short>.jsonl`
//! (see [`record_agent_dispatch`]), which is how a nested subagent finds its parent: claude's own
//! `.meta.json` names the dispatching `toolUseId` but never the caller. Every dashboard pane and
//! `zirv agent` delegation writes `<state>/graph-launch/<session short>.json` once, so a pane
//! keeps its parent, task and workflow after its session record is swept.
//!
//! Native subagents are also discovered read-side from claude's own
//! `<transcript dir>/<session id>/subagents/agent-<id>.meta.json` files, so a session launched
//! without the SubagentStart hook still shows its agents; a hook record wins on conflict.
//!
//! Read side ([`snapshot`], [`merged_events`]) runs only when a reader asks.
//! It merges the session registry (without sweeping it), delegations, work
//! groups, subagent nodes and Codex sub-agent rollouts into one flat list of
//! [`Node`]s linked by `parent`, and the existing logs into one time-ordered
//! [`Event`] list. Nothing here changes how those logs are written.
//!
//! Codex children are read from rollouts under `~/.codex/sessions`, verified
//! against codex-cli 0.155.1 and 0.159.2: a child's first line is a
//! `session_meta` carrying `parent_thread_id` and `source.subagent.
//! thread_spawn {depth, agent_path, agent_nickname, agent_role}`; model and
//! effort come from `turn_context`, tokens from the last `token_count`, and
//! status from `task_started`/`task_complete`/`turn_aborted`. Guardian review
//! threads (`source.subagent.other`) are not agents and are skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::adapters::{AgentAdapter, SESSION_ENV};
use super::config::{EnvLookup, env_from_process};
pub use super::graph_steps::Step;
use super::sessions;
use super::state::{
    StateDir, create_private_dir_all, now_secs, open_private_append, prune_to_newest, write_private,
};
use super::{delegation, group, log, session_spend};

const GRAPH_DIR: &str = "graph";
const SCHEMA_VERSION: u32 = 1;
/// Node files kept per session directory.
const KEEP_NODES_PER_SESSION: usize = 200;
/// A node directory with no registered session is dropped once idle this long.
const PRUNE_INTERVAL_SECS: u64 = 600;
const PRUNE_MARKER: &str = "subagent-graph.pruned";
const NODE_DIR_MAX_IDLE_SECS: u64 = 7 * 86_400;
/// Codex rollouts older than this are not scanned for children.
const CODEX_WINDOW_SECS: u64 = 7 * 86_400;
const CODEX_MAX_FILES: usize = 2_000;
/// Lines taken from each event source before the merge.
const EVENTS_PER_SOURCE: usize = 500;
const JSONL_TAIL_BYTES: u64 = 256 * 1024;
const JOB_CHARS: usize = 80;
/// The sender `supervisor::deliver_mail` mails its advice from.
const SUPERVISOR_SENDER: &str = "supervisor";
const DISPATCH_DIR: &str = "graph-dispatch";
const LAUNCH_DIR: &str = "graph-launch";
const KEEP_DISPATCH_FILES: usize = 200;
const KEEP_LAUNCH_FILES: usize = 500;
/// Spend rows read to place panes that predate their launch record.
const SPEND_ROWS: usize = 5_000;
/// Sessions older than this (and not alive) are not scanned for native subagents.
const NATIVE_WINDOW_SECS: u64 = 7 * 86_400;

/// One agent in the graph. `parent` names another node's `id`; a parent that
/// is not itself a node makes this a root.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    /// `session`, `delegation`, `group`, `subagent` or `codex_child`.
    pub kind: String,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub role: Option<String>,
    pub status: String,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    pub tokens: Option<u64>,
    pub label: Option<String>,
    /// What this agent was asked to do: first line, redacted, at most 80 characters. `None`
    /// when nothing recorded it.
    pub job: Option<String>,
    /// The zirv workflow step this agent was dispatched under (or, for a live session, is on).
    pub workflow: Option<WorkflowStamp>,
    /// The topmost `session` node this node hangs under; `None` for an orphan.
    pub session: Option<String>,
    /// The agent's last tool calls (at most 5, newest last), read from its own transcript or rollout.
    pub steps: Vec<Step>,
}

/// A workflow run and the step it was on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStamp {
    pub id: String,
    pub pack: String,
    pub step: String,
}

/// One row of the merged log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub ts: u64,
    pub actor: String,
    pub kind: String,
    pub summary: String,
    pub p: Option<f64>,
    /// Recipient of a `mail` or `supervisor` event (a session short id or a role), or the agent id of a
    /// `subagent_start`; absent for every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// The durable record of one native Claude subagent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct SubagentRecord {
    session: String,
    agent_id: String,
    agent_type: String,
    /// The `Agent` tool call that dispatched this agent; with the dispatch log it names the parent.
    tool_use_id: Option<String>,
    description: Option<String>,
    requested_model: Option<String>,
    model: Option<String>,
    status: String,
    started_at: u64,
    ended_at: Option<u64>,
    input_tokens: u64,
    output_tokens: u64,
}

/// The fields of a SubagentStart/SubagentStop payload this module reads.
struct HookFields {
    session_id: String,
    agent_id: String,
    agent_type: String,
    transcript_path: String,
    agent_transcript_path: String,
    tool_use_id: String,
}

fn parse_hook(stdin: &str) -> Option<HookFields> {
    let value: Value = serde_json::from_str(stdin).ok()?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let fields = HookFields {
        session_id: text("session_id"),
        agent_id: text("agent_id"),
        agent_type: text("agent_type"),
        transcript_path: text("transcript_path"),
        agent_transcript_path: text("agent_transcript_path"),
        tool_use_id: text("tool_use_id"),
    };
    (!fields.agent_id.is_empty()).then_some(fields)
}

fn file_safe(raw: &str) -> String {
    raw.chars()
        .take(128)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn graph_root(state: &StateDir) -> PathBuf {
    state.root().join(GRAPH_DIR)
}

fn node_path(state: &StateDir, session: &str, agent_id: &str) -> PathBuf {
    graph_root(state)
        .join(sessions::short_id(session))
        .join(format!("{}.json", file_safe(agent_id)))
}

/// The zirv session when the hook runs under one, else claude's own id.
fn hook_session(claude_session_id: &str, env: EnvLookup<'_>) -> String {
    env(SESSION_ENV)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| claude_session_id.to_string())
}

fn subagent_transcript(fields: &HookFields) -> Option<PathBuf> {
    if !fields.agent_transcript_path.is_empty() {
        return Some(PathBuf::from(&fields.agent_transcript_path));
    }
    let lead = Path::new(&fields.transcript_path);
    if fields.transcript_path.is_empty() {
        return None;
    }
    Some(
        super::adapters::claude::subagents_dir(lead)?
            .join(format!("agent-{}.jsonl", fields.agent_id)),
    )
}

/// Fill description, requested model and dispatching tool call from the `.meta.json`
/// claude writes beside a subagent transcript.
fn apply_meta(record: &mut SubagentRecord, transcript: Option<&Path>) {
    let Some(meta) = transcript
        .map(|path| path.with_extension("meta.json"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return;
    };
    let text = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    record.description = text("description").or(record.description.take());
    record.requested_model = text("model").or(record.requested_model.take());
    record.tool_use_id = text("toolUseId").or(record.tool_use_id.take());
    if record.agent_type.is_empty() {
        record.agent_type = text("agentType").unwrap_or_default();
    }
}

fn save(state: &StateDir, record: &SubagentRecord) {
    let path = node_path(state, &record.session, &record.agent_id);
    if let Some(dir) = path.parent() {
        let _ = create_private_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(record) {
        let _ = write_private(&path, &json);
    }
}

/// `zirv ctx hook subagent-start`: record a node. Always exit 0, no output.
pub fn run_subagent_start(stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    let Some(fields) = parse_hook(stdin) else {
        return Ok(0);
    };
    let Ok(state) = StateDir::resolve(env) else {
        return Ok(0);
    };
    let session = hook_session(&fields.session_id, env);
    if session.is_empty() {
        return Ok(0);
    }
    let now = now_secs();
    prune_if_due(&state, now);
    let mut record = SubagentRecord {
        session,
        agent_id: fields.agent_id.clone(),
        agent_type: fields.agent_type.clone(),
        status: "running".to_string(),
        started_at: now,
        tool_use_id: Some(fields.tool_use_id.clone()).filter(|id| !id.is_empty()),
        ..SubagentRecord::default()
    };
    apply_meta(&mut record, subagent_transcript(&fields).as_deref());
    save(&state, &record);
    Ok(0)
}

/// Called from the SubagentStop hook before its own gate: close the node with
/// its status, actual model and tokens. Output-free and infallible.
pub fn record_subagent_stop(stdin: &str, env: EnvLookup<'_>) {
    let Some(fields) = parse_hook(stdin) else {
        return;
    };
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    let session = hook_session(&fields.session_id, env);
    if session.is_empty() {
        return;
    }
    let now = now_secs();
    let mut record = std::fs::read_to_string(node_path(&state, &session, &fields.agent_id))
        .ok()
        .and_then(|text| serde_json::from_str::<SubagentRecord>(&text).ok())
        .unwrap_or_else(|| SubagentRecord {
            session,
            agent_id: fields.agent_id.clone(),
            agent_type: fields.agent_type.clone(),
            started_at: now,
            ..SubagentRecord::default()
        });
    let transcript = subagent_transcript(&fields);
    apply_meta(&mut record, transcript.as_deref());
    record.status = "completed".to_string();
    record.ended_at = Some(now);
    if let Some(path) = transcript {
        apply_usage(&mut record, &path);
    }
    save(&state, &record);
}

/// Tokens and the most-used model of a finished subagent transcript.
fn apply_usage(record: &mut SubagentRecord, transcript: &Path) {
    let fold = session_spend::subagent_transcript_usage(transcript);
    record.input_tokens = fold.buckets.iter().map(|b| b.usage.context_total()).sum();
    record.output_tokens = fold.buckets.iter().map(|b| b.usage.output_tokens).sum();
    record.model = fold
        .buckets
        .iter()
        .filter(|b| b.model.is_some())
        .max_by_key(|b| b.messages)
        .and_then(|b| b.model.clone())
        .or(record.model.take());
}

// -- Job text, dispatch and launch records ----------------------------------------

/// First line of `raw`, redacted and capped at `JOB_CHARS`: the job every node shows. `None`
/// for blank text.
pub(super) fn job_text(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let bounded: String = line.chars().take(JOB_CHARS * 5).collect();
    let clean: String = super::snapshot::redact_text(&bounded)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return None;
    }
    if clean.chars().count() <= JOB_CHARS {
        return Some(clean.to_string());
    }
    let mut cut: String = clean.chars().take(JOB_CHARS - 1).collect();
    cut.push('\u{2026}');
    Some(cut)
}

/// The repo's active workflow run and its current step, read as `zirv workflow status` reads it.
fn active_workflow(state: &StateDir, repo: &Path) -> Option<WorkflowStamp> {
    let (id, pack, step) = crate::commands::workflow::active_workflow_stamp(state, repo)?;
    Some(WorkflowStamp { id, pack, step })
}

/// One `Agent` dispatch: which tool call, who made it and the workflow step it ran under.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct DispatchLine {
    tool_use_id: String,
    caller_agent_id: Option<String>,
    ts: u64,
    workflow: Option<WorkflowStamp>,
}

fn dispatch_path(state: &StateDir, session: &str) -> PathBuf {
    state
        .root()
        .join(DISPATCH_DIR)
        .join(format!("{}.jsonl", file_safe(&sessions::short_id(session))))
}

/// `PreToolUse(Agent|Task)`: append one line naming the call, its caller and the workflow
/// step. Output-free and infallible; one small append per dispatch.
pub fn record_agent_dispatch(
    env: EnvLookup<'_>,
    claude_session_id: &str,
    tool_use_id: &str,
    caller_agent_id: &str,
    repo: Option<&Path>,
) {
    if tool_use_id.is_empty() {
        return;
    }
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    let session = hook_session(claude_session_id, env);
    if session.is_empty() {
        return;
    }
    let line = DispatchLine {
        tool_use_id: tool_use_id.to_string(),
        caller_agent_id: Some(caller_agent_id.to_string()).filter(|id| !id.is_empty()),
        ts: now_secs(),
        workflow: repo.and_then(|repo| active_workflow(&state, repo)),
    };
    let path = dispatch_path(&state, &session);
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = create_private_dir_all(dir);
    if let (Ok(json), Ok(mut file)) = (serde_json::to_string(&line), open_private_append(&path)) {
        let _ = writeln!(file, "{json}");
    }
    prune_to_newest(dir, KEEP_DISPATCH_FILES);
}

/// Dispatch lines of one session by tool call id.
fn read_dispatch(state: &StateDir, session: &str) -> BTreeMap<String, DispatchLine> {
    jsonl_tail(&dispatch_path(state, session))
        .into_iter()
        .filter_map(|row| serde_json::from_value::<DispatchLine>(row).ok())
        .filter(|line| !line.tool_use_id.is_empty())
        .map(|line| (line.tool_use_id.clone(), line))
        .collect()
}

/// What a worker launch leaves behind so the worker keeps its place in the graph after its
/// session record is swept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct LaunchRecord {
    session: String,
    /// `pane` (a dashboard pane) or `delegation` (a `zirv agent` run).
    origin: String,
    parent_session: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    /// The task's first line, redacted and capped.
    task: Option<String>,
    workdir: Option<PathBuf>,
    started_at: u64,
    workflow: Option<WorkflowStamp>,
}

/// The facts a launch site knows; [`record_worker_launch`] adds the time and workflow stamp.
pub(super) struct Launch<'a> {
    pub session: &'a str,
    pub origin: &'a str,
    pub parent_session: Option<&'a str>,
    pub harness: Option<&'a str>,
    pub model: Option<&'a str>,
    pub task: Option<&'a str>,
    pub workdir: Option<&'a Path>,
}

/// Write `<state>/graph-launch/<session>.json` for a worker that was just launched. Best-effort.
pub(super) fn record_worker_launch(state: &StateDir, repo: &Path, launch: &Launch<'_>, now: u64) {
    if launch.session.is_empty() {
        return;
    }
    let record = LaunchRecord {
        session: launch.session.to_string(),
        origin: launch.origin.to_string(),
        parent_session: launch
            .parent_session
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        harness: launch.harness.map(str::to_string),
        model: launch.model.map(str::to_string),
        task: launch.task.and_then(job_text),
        workdir: launch.workdir.map(Path::to_path_buf),
        started_at: now,
        workflow: active_workflow(state, repo),
    };
    let dir = state.root().join(LAUNCH_DIR);
    let _ = create_private_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(&record) {
        let _ = write_private(
            &dir.join(format!("{}.json", file_safe(launch.session))),
            &json,
        );
    }
    prune_to_newest(&dir, KEEP_LAUNCH_FILES);
}

type LaunchCache = std::sync::Mutex<BTreeMap<PathBuf, LaunchRecord>>;

/// Launch records by path; they are written once, so a cached path is never read again.
fn launch_cache() -> &'static LaunchCache {
    static CACHE: std::sync::OnceLock<LaunchCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn read_launch_records(state: &StateDir) -> Vec<LaunchRecord> {
    let Ok(entries) = std::fs::read_dir(state.root().join(LAUNCH_DIR)) else {
        return Vec::new();
    };
    let paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    let mut cache = launch_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|path, _| paths.contains(path));
    for path in paths {
        if cache.contains_key(&path) {
            continue;
        }
        if let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<LaunchRecord>(&text).ok())
            .filter(|record| !record.session.is_empty())
        {
            cache.insert(path, record);
        }
    }
    cache.values().cloned().collect()
}

// -- Native subagents from claude's own files ---------------------------------

fn secs_of(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The newest parseable line of a transcript, read from its last `JSONL_TAIL_BYTES`.
fn last_row(path: &Path) -> Option<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(JSONL_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // A read that starts mid-file begins inside a line.
    if start > 0 {
        lines.next();
    }
    lines
        .rev()
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| serde_json::from_str(line).ok())
}

fn first_row_secs(path: &Path) -> Option<u64> {
    let mut first = String::new();
    std::io::BufReader::new(std::fs::File::open(path).ok()?)
        .read_line(&mut first)
        .ok()?;
    rollout_ts(&serde_json::from_str::<Value>(&first).ok()?)
}

/// A subagent transcript ends with the tool result that ends its turn (`toolEndsTurn`, as
/// `SubagentHandback` does) or with an assistant message that stopped on `end_turn`.
fn row_ends_agent(row: &Value) -> bool {
    row.get("toolEndsTurn").and_then(Value::as_bool) == Some(true)
        || row.pointer("/message/stop_reason").and_then(Value::as_str) == Some("end_turn")
}

type NativeKey = (std::time::SystemTime, u64, std::time::SystemTime);

#[derive(Clone)]
struct NativeEntry {
    key: NativeKey,
    started_at: u64,
    record: SubagentRecord,
}

type NativeCache = std::sync::Mutex<BTreeMap<PathBuf, NativeEntry>>;

/// Parsed native agents by meta path; an unchanged (transcript mtime, len, meta mtime) is a hit.
fn native_cache() -> &'static NativeCache {
    static CACHE: std::sync::OnceLock<NativeCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn read_native(
    meta_path: &Path,
    transcript: &Path,
    id: &str,
    session: &str,
    known_start: Option<u64>,
    meta_modified: std::time::SystemTime,
) -> Option<(u64, SubagentRecord)> {
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(meta_path).ok()?).ok()?;
    let text = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let last = last_row(transcript);
    let done = last.as_ref().is_some_and(row_ends_agent);
    let started_at = known_start
        .or_else(|| first_row_secs(transcript))
        .unwrap_or_else(|| secs_of(meta_modified));
    let mut record = SubagentRecord {
        session: session.to_string(),
        agent_id: id.to_string(),
        agent_type: text("agentType").unwrap_or_default(),
        tool_use_id: text("toolUseId"),
        description: text("description"),
        requested_model: text("model"),
        status: if done { "completed" } else { "running" }.to_string(),
        started_at,
        ..SubagentRecord::default()
    };
    if done {
        record.ended_at = last.as_ref().and_then(rollout_ts).or(Some(started_at));
        apply_usage(&mut record, transcript);
    }
    Some((started_at, record))
}

/// The native agents claude wrote under one session's `subagents/` directory, hook or not. A
/// running agent whose session is gone is `stopped`: nothing can still be running it.
fn native_subagents(dir: &Path, session: &str, session_alive: bool) -> Vec<SubagentRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut cache = native_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path();
        let Some(id) = meta_path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("agent-"))
            .and_then(|n| n.strip_suffix(".meta.json"))
            .map(str::to_string)
        else {
            continue;
        };
        let transcript = dir.join(format!("agent-{id}.jsonl"));
        let Some(meta_modified) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        let transcript_stat = std::fs::metadata(&transcript)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())))
            .unwrap_or((std::time::UNIX_EPOCH, 0));
        let key = (transcript_stat.0, transcript_stat.1, meta_modified);
        let cached = cache.get(&meta_path).cloned();
        let entry = match cached {
            Some(hit) if hit.key == key => hit,
            prior => {
                let Some((started_at, record)) = read_native(
                    &meta_path,
                    &transcript,
                    &id,
                    session,
                    prior.map(|p| p.started_at),
                    meta_modified,
                ) else {
                    continue;
                };
                let fresh = NativeEntry {
                    key,
                    started_at,
                    record,
                };
                cache.insert(meta_path.clone(), fresh.clone());
                fresh
            }
        };
        let mut record = entry.record;
        if record.status == "running" && !session_alive {
            record.status = "stopped".to_string();
        }
        found.push(record);
    }
    found
}

/// [`prune`] at most once per `PRUNE_INTERVAL_SECS`, paced by a marker file's mtime, so the
/// common SubagentStart is a single small write.
fn prune_if_due(state: &StateDir, now: u64) {
    let marker = state.root().join(PRUNE_MARKER);
    let recent = std::fs::metadata(&marker)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|at| now.saturating_sub(at.as_secs()) < PRUNE_INTERVAL_SECS);
    if recent || !graph_root(state).is_dir() {
        return;
    }
    let _ = write_private(&marker, &now.to_string());
    prune(state, now);
}

/// Bound node storage: cap each session directory, and drop a directory whose
/// session is no longer registered once it has been idle for a week.
fn prune(state: &StateDir, now: u64) {
    let Ok(entries) = std::fs::read_dir(graph_root(state)) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let Some(short) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        prune_to_newest(&dir, KEEP_NODES_PER_SESSION);
        if sessions::load_record(state, &short).is_some() {
            continue;
        }
        let idle = std::fs::metadata(&dir)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|age| now.saturating_sub(age.as_secs()) > NODE_DIR_MAX_IDLE_SECS);
        if idle {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

fn read_subagent_records(state: &StateDir) -> Vec<SubagentRecord> {
    let mut found = Vec::new();
    let Ok(dirs) = std::fs::read_dir(graph_root(state)) else {
        return found;
    };
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<SubagentRecord>(&text).ok())
                .filter(|record| !record.agent_id.is_empty())
            {
                found.push(record);
            }
        }
    }
    found
}

// -- Codex children --------------------------------------------------------

fn rollout_ts(row: &Value) -> Option<u64> {
    row.get("timestamp")
        .and_then(Value::as_str)
        .and_then(super::window::parse_iso8601_utc)
}

/// A Codex sub-agent rollout as a node whose `parent` is the raw parent
/// thread id. `None` for anything that is not a `thread_spawn` child.
/// The node a rollout's first line (`session_meta`) describes; `None` for a guardian thread.
fn codex_head(first_line: &str) -> Option<Node> {
    let first: Value = serde_json::from_str(first_line).ok()?;
    if first.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let meta = first.get("payload")?;
    let parent = meta.get("parent_thread_id").and_then(Value::as_str)?;
    let spawn = meta.pointer("/source/subagent/thread_spawn")?;
    let id = meta.get("id").and_then(Value::as_str)?;
    let role = spawn
        .get("agent_role")
        .and_then(Value::as_str)
        .or_else(|| {
            spawn
                .get("agent_path")
                .and_then(Value::as_str)
                .and_then(|path| path.rsplit('/').next())
        })
        .map(str::to_string);
    Some(Node {
        id: id.to_string(),
        parent: Some(parent.to_string()),
        kind: "codex_child".to_string(),
        harness: Some("codex".to_string()),
        model: None,
        effort: None,
        role,
        status: "running".to_string(),
        started_at: meta
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(super::window::parse_iso8601_utc),
        ended_at: None,
        tokens: None,
        label: spawn
            .get("agent_nickname")
            .and_then(Value::as_str)
            .map(str::to_string),
        job: None,
        workflow: None,
        session: None,
        steps: Vec::new(),
    })
}

/// The task a child was given, when its rollout holds it in the clear: an `event_msg`
/// `user_message`, or a user `message` item that is not injected context (those start with `<`).
/// codex-cli 0.159 sends a child's task as `encrypted_content`, so a child may have none.
fn codex_user_job(row: &Value) -> Option<String> {
    let payload = row.get("payload")?;
    match (
        row.get("type").and_then(Value::as_str)?,
        payload.get("type").and_then(Value::as_str),
    ) {
        ("event_msg", Some("user_message")) => payload
            .get("message")
            .and_then(Value::as_str)
            .and_then(job_text),
        ("response_item", Some("message"))
            if payload.get("role").and_then(Value::as_str) == Some("user") =>
        {
            payload
                .get("content")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.trim_start().starts_with('<'))
                .find_map(job_text)
        }
        _ => None,
    }
}

/// Folds one rollout line into `node`.
fn apply_codex_line(node: &mut Node, line: &str) {
    let wants_job =
        node.job.is_none() && (line.contains("user_message") || line.contains(r#""role":"user""#));
    if !wants_job
        && ![
            "turn_context",
            "token_count",
            "task_started",
            "task_complete",
            "turn_aborted",
        ]
        .iter()
        .any(|marker| line.contains(marker))
    {
        return;
    }
    let Ok(row) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if wants_job {
        node.job = codex_user_job(&row);
    }
    let payload = row.get("payload").unwrap_or(&Value::Null);
    match (
        row.get("type").and_then(Value::as_str),
        payload.get("type").and_then(Value::as_str),
    ) {
        (Some("turn_context"), _) => {
            let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
            node.model = text("model").or(node.model.take());
            node.effort = text("effort").or(node.effort.take());
        }
        (_, Some("token_count")) => {
            let usage = payload.pointer("/info/total_token_usage");
            let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
            node.tokens = count("total_tokens")
                .or_else(|| Some(count("input_tokens")? + count("output_tokens").unwrap_or(0)))
                .or(node.tokens);
        }
        (_, Some("task_started")) => {
            node.status = "running".to_string();
            node.ended_at = None;
        }
        (_, Some(done @ ("task_complete" | "turn_aborted"))) => {
            node.status = if done == "task_complete" {
                "completed"
            } else {
                "aborted"
            }
            .to_string();
            node.ended_at = rollout_ts(&row);
        }
        _ => {}
    }
}

fn parse_codex_child(text: &str) -> Option<Node> {
    let mut lines = text.lines();
    let mut node = codex_head(lines.next()?)?;
    for line in lines {
        apply_codex_line(&mut node, line);
    }
    Some(node)
}

fn collect_rollouts(dir: &Path, depth: u32, cutoff: std::time::SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= CODEX_MAX_FILES {
            return;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        // A date directory untouched since before the window holds no live rollout.
        if meta.is_dir() && depth < 3 {
            if meta.modified().is_ok_and(|modified| modified < cutoff) {
                continue;
            }
            collect_rollouts(&path, depth + 1, cutoff, out);
        } else if meta.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && meta.modified().is_ok_and(|modified| modified >= cutoff)
        {
            out.push(path);
        }
    }
}

fn first_line_is_codex_child(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).is_ok()
        && first.contains("thread_spawn")
        && first.contains("parent_thread_id")
}

type RolloutKey = (std::time::SystemTime, u64);

/// What is known of one rollout: the node folded from its complete lines
/// (`committed`, valid up to byte `offset`) and the node including a trailing
/// unterminated line (`output`).
#[derive(Clone, Default)]
struct RolloutState {
    key: Option<RolloutKey>,
    id: Option<u64>,
    offset: u64,
    rejected: bool,
    committed: Option<Node>,
    output: Option<Node>,
}

type RolloutCache = std::sync::Mutex<BTreeMap<PathBuf, RolloutState>>;

/// Parsed rollouts by path; an unchanged (mtime, len) is a hit and a grown file parses only
/// its appended bytes.
fn rollout_cache() -> &'static RolloutCache {
    static CACHE: std::sync::OnceLock<RolloutCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

#[cfg(test)]
static ROLLOUT_PARSES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn file_id(meta: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Brings `prior` up to the file's current content, reading only bytes past its offset. A
/// shrunk or replaced file restarts from byte 0. The result equals `parse_codex_child` of
/// the whole file.
fn read_codex_child(
    path: &Path,
    prior: Option<RolloutState>,
    key: RolloutKey,
    id: Option<u64>,
) -> RolloutState {
    use std::io::{Read, Seek, SeekFrom};

    #[cfg(test)]
    ROLLOUT_PARSES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut state = match prior {
        Some(prior) if prior.id == id && prior.offset <= key.1 => prior,
        _ => RolloutState::default(),
    };
    state.key = Some(key);
    state.id = id;
    if state.rejected {
        return state;
    }
    if state.offset == 0 && !first_line_is_codex_child(path) {
        state.output = None;
        return state;
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(state.offset))?;
        file.read_to_end(&mut bytes)
    });
    if read.is_err() {
        return RolloutState::default();
    }
    let complete_len = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let complete = String::from_utf8_lossy(&bytes[..complete_len]);
    let tail = String::from_utf8_lossy(&bytes[complete_len..]);
    let mut lines = complete.lines();
    if state.offset == 0
        && let Some(first) = lines.next()
    {
        state.committed = codex_head(first);
        state.rejected = state.committed.is_none();
    }
    if let Some(node) = state.committed.as_mut() {
        for line in lines {
            apply_codex_line(node, line);
        }
    }
    state.offset += complete_len as u64;
    state.output = match (&state.committed, state.rejected) {
        (_, true) => None,
        (Some(node), false) => {
            let mut node = node.clone();
            apply_codex_line(&mut node, &tail);
            Some(node)
        }
        (None, false) => parse_codex_child(&tail),
    };
    state
}

fn codex_children(root: &Path, now: u64) -> Vec<Node> {
    let cutoff = std::time::UNIX_EPOCH
        + std::time::Duration::from_secs(now.saturating_sub(CODEX_WINDOW_SECS));
    let mut files = Vec::new();
    collect_rollouts(root, 0, cutoff, &mut files);
    let live: std::collections::HashSet<&PathBuf> = files.iter().collect();
    let mut cache = rollout_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|path, _| live.contains(path));
    let mut nodes = Vec::new();
    for path in &files {
        let Some((key, id)) = std::fs::metadata(path)
            .ok()
            .and_then(|m| Some(((m.modified().ok()?, m.len()), file_id(&m))))
        else {
            continue;
        };
        let state = match cache.get(path) {
            Some(cached) if cached.key == Some(key) => cached.clone(),
            _ => {
                let state = read_codex_child(path, cache.get(path).cloned(), key, id);
                cache.insert(path.clone(), state.clone());
                state
            }
        };
        nodes.extend(state.output.map(|mut node| {
            node.steps = super::graph_steps::latest(path);
            node
        }));
    }
    nodes
}

/// Each Jev site's newest decision: `(margin, sharp)`. Sharp means no fallback
/// and a margin at or above the default floor (per-site floors are not recorded).
pub(super) fn jev_site_verdicts(state: &StateDir) -> BTreeMap<String, (f64, bool)> {
    let mut newest: BTreeMap<String, (u64, f64, bool)> = BTreeMap::new();
    for row in jsonl_tail(&state.root().join("jev-decisions.jsonl")) {
        let (Some(ts), site) = (row.get("ts").and_then(Value::as_u64), str_of(&row, "site")) else {
            continue;
        };
        let Some(margin) = row.get("answers").and_then(Value::as_object).and_then(|a| {
            min_f64(
                a.values()
                    .filter_map(|v| v.get("margin").and_then(Value::as_f64)),
            )
        }) else {
            continue;
        };
        let fell_back = row
            .get("fallbacks")
            .and_then(Value::as_array)
            .is_some_and(|f| !f.is_empty());
        let sharp = !fell_back && margin >= f64::from(super::jev::DEFAULT_MIN_MARGIN);
        if newest.get(site).is_none_or(|(t, _, _)| ts >= *t) {
            newest.insert(site.to_string(), (ts, margin, sharp));
        }
    }
    newest
        .into_iter()
        .map(|(k, (_, m, s))| (k, (m, s)))
        .collect()
}

/// `thread id -> zirv session` from the rollout pointer files
/// (`<state>/rollouts/<short>.path`), whose content is a rollout path ending
/// in the thread id.
fn codex_thread_owners(
    state: &StateDir,
    short_to_session: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut owners = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(state.rollouts()) else {
        return owners;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(short) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".path"))
        else {
            continue;
        };
        let (Some(session), Ok(pointer)) =
            (short_to_session.get(short), std::fs::read_to_string(&path))
        else {
            continue;
        };
        let stem = Path::new(pointer.trim())
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if stem.len() >= 36 {
            owners.insert(stem[stem.len() - 36..].to_string(), session.clone());
        }
    }
    owners
}

// -- Snapshot ---------------------------------------------------------------

pub(super) fn read_session_records(state: &StateDir) -> Vec<(sessions::Record, bool)> {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|path| {
            let record =
                serde_json::from_str::<sessions::Record>(&std::fs::read_to_string(path).ok()?)
                    .ok()?;
            let alive = sessions::record_is_alive(&record);
            Some((record, alive))
        })
        .collect()
}

/// Merge every source into one flat node list, oldest first. Read-only: it
/// does not sweep the session registry. `repo` scopes delegations; sessions,
/// groups and subagents are machine-wide. Every node carries the `session` it
/// hangs under, so a caller can scope the list to one session or repository.
pub fn snapshot(state: &StateDir, repo: &Path, codex_root: Option<&Path>, now: u64) -> Vec<Node> {
    snapshot_in(state, repo, codex_root, None, now)
}

/// [`snapshot`] with claude's home directory named (`None` is the process's own).
fn snapshot_in(
    state: &StateDir,
    repo: &Path,
    codex_root: Option<&Path>,
    claude_home: Option<&Path>,
    now: u64,
) -> Vec<Node> {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut alias: BTreeMap<String, String> = BTreeMap::new();
    let mut short_to_session: BTreeMap<String, String> = BTreeMap::new();
    let mut repo_workflows: BTreeMap<PathBuf, Option<WorkflowStamp>> = BTreeMap::new();

    let session_records = read_session_records(state);
    for (record, alive) in &session_records {
        alias.insert(record.session.clone(), record.session.clone());
        alias.insert(record.short.clone(), record.session.clone());
        short_to_session.insert(record.short.clone(), record.session.clone());
        let status = if *alive {
            "live"
        } else if record.in_flight.is_some() {
            "crashed"
        } else {
            "ended"
        };
        // A live session is on whatever step its repository's active workflow is on.
        let workflow = alive
            .then(|| {
                repo_workflows
                    .entry(record.repo.clone())
                    .or_insert_with(|| active_workflow(state, &record.repo))
                    .clone()
            })
            .flatten();
        nodes.insert(
            record.session.clone(),
            Node {
                id: record.session.clone(),
                parent: None,
                kind: "session".to_string(),
                harness: Some(record.agent.clone()),
                model: None,
                effort: None,
                role: record.role.clone(),
                status: status.to_string(),
                started_at: Some(record.started_at),
                ended_at: None,
                tokens: None,
                label: Some(format!("{} {}", record.short, record.verb)),
                job: None,
                workflow,
                session: None,
                steps: Vec::new(),
            },
        );
    }
    let resolve = |raw: &str| alias.get(raw).cloned().unwrap_or_else(|| raw.to_string());

    let groups = group::list(state);
    for g in &groups {
        let id = format!("group:{}", g.work_group_id);
        nodes.insert(
            id.clone(),
            Node {
                id,
                parent: Some(resolve(&g.parent_session_id)),
                kind: "group".to_string(),
                harness: None,
                model: None,
                effort: None,
                role: Some(g.scope.clone()),
                status: if g.closed_at.is_some() {
                    "closed"
                } else {
                    "open"
                }
                .to_string(),
                started_at: Some(g.created_at),
                ended_at: g.closed_at,
                tokens: Some(g.spent_tokens),
                label: None,
                job: None,
                workflow: None,
                session: None,
                steps: Vec::new(),
            },
        );
    }
    let group_ids: BTreeSet<&str> = groups.iter().map(|g| g.work_group_id.as_str()).collect();

    for record in delegation::list(state, repo) {
        let parent = record
            .handle
            .group
            .as_deref()
            .filter(|id| group_ids.contains(id))
            .map(|id| format!("group:{id}"))
            .or_else(|| record.parent_session.as_deref().map(resolve));
        let ended = record.phase.is_terminal().then_some(record.updated_at);
        let worker = record.handle.worker_session.clone();
        let node = nodes.entry(worker.clone()).or_insert_with(|| Node {
            id: worker,
            parent: None,
            kind: "delegation".to_string(),
            harness: None,
            model: None,
            effort: None,
            role: Some(record.handle.role.clone()),
            status: record.phase.as_str().to_string(),
            started_at: Some(record.launched_at),
            ended_at: ended,
            tokens: None,
            label: Some(record.handle.short.clone()),
            job: None,
            workflow: None,
            session: None,
            steps: Vec::new(),
        });
        node.parent = parent;
        if node.status != "live" {
            node.status = record.phase.as_str().to_string();
            node.ended_at = ended;
        }
        node.role = node.role.take().or(Some(record.handle.role.clone()));
        node.job = record
            .handle
            .objective
            .as_deref()
            .and_then(job_text)
            .or_else(|| record.handle.task.as_deref().and_then(job_text));
    }

    place_subagents(state, &session_records, claude_home, &resolve, &mut nodes);
    place_workers(state, &resolve, &mut nodes);
    place_session_steps(state, &session_records, claude_home, &mut nodes);

    if let Some(root) = codex_root {
        let owners = codex_thread_owners(state, &short_to_session);
        for mut child in codex_children(root, now) {
            child.parent = child
                .parent
                .map(|thread| owners.get(&thread).cloned().unwrap_or(thread));
            nodes.insert(child.id.clone(), child);
        }
    }

    attribute_sessions(&mut nodes);
    let mut ordered: Vec<Node> = nodes.into_values().collect();
    ordered.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    ordered
}

/// Where claude keeps one native subagent's transcript, for the session that ran it.
pub(super) fn native_subagent_path(
    state: &StateDir,
    session: &str,
    agent_id: &str,
) -> Option<PathBuf> {
    let (record, _) = read_session_records(state)
        .into_iter()
        .find(|(record, _)| record.session == session)?;
    let transcript = super::adapters::claude::ClaudeAdapter::new(None).transcript_path(
        &super::event::SessionRef {
            id: super::event::SessionId::parse(&record.session),
            cwd: record.repo.clone(),
        },
    );
    let dir = super::adapters::claude::subagents_dir(&transcript)?;
    Some(dir.join(format!("agent-{}.jsonl", file_safe(agent_id))))
}

/// Native subagents: claude's own `subagents/` files merged with the hook records (the hook
/// record wins), each parented on the caller of its dispatching `Agent` call, else the session.
fn place_subagents(
    state: &StateDir,
    session_records: &[(sessions::Record, bool)],
    claude_home: Option<&Path>,
    resolve: &dyn Fn(&str) -> String,
    nodes: &mut BTreeMap<String, Node>,
) {
    let mut adapter = super::adapters::claude::ClaudeAdapter::new(None);
    if let Some(home) = claude_home {
        adapter = adapter.with_home(home.to_path_buf());
    }
    let mut agents: BTreeMap<String, SubagentRecord> = BTreeMap::new();
    let mut agent_dirs: BTreeMap<String, PathBuf> = BTreeMap::new();
    let cutoff = now_secs().saturating_sub(NATIVE_WINDOW_SECS);
    for (record, alive) in session_records {
        if record.agent != "claude" || (!alive && record.started_at < cutoff) {
            continue;
        }
        let transcript = adapter.transcript_path(&super::event::SessionRef {
            id: super::event::SessionId::parse(&record.session),
            cwd: record.repo.clone(),
        });
        let Some(dir) = super::adapters::claude::subagents_dir(&transcript) else {
            continue;
        };
        for found in native_subagents(&dir, &record.session, *alive) {
            agents.insert(found.agent_id.clone(), found);
        }
        agent_dirs.insert(record.session.clone(), dir);
    }
    for mut hooked in read_subagent_records(state) {
        if let Some(native) = agents.remove(&hooked.agent_id) {
            hooked.tool_use_id = hooked.tool_use_id.or(native.tool_use_id);
            hooked.description = hooked.description.or(native.description);
            hooked.requested_model = hooked.requested_model.or(native.requested_model);
            // A session that died before SubagentStop leaves the hook record running for good.
            if hooked.status == "running" && native.status != "running" {
                hooked.status = native.status;
                hooked.ended_at = hooked.ended_at.or(native.ended_at);
                hooked.input_tokens = hooked.input_tokens.max(native.input_tokens);
                hooked.output_tokens = hooked.output_tokens.max(native.output_tokens);
            }
        }
        agents.insert(hooked.agent_id.clone(), hooked);
    }

    let mut dispatches: BTreeMap<String, BTreeMap<String, DispatchLine>> = BTreeMap::new();
    let ids: BTreeSet<String> = agents.keys().cloned().collect();
    let live: BTreeSet<&str> = session_records
        .iter()
        .filter(|(_, alive)| *alive)
        .map(|(record, _)| record.session.as_str())
        .collect();
    for record in agents.into_values() {
        let session = resolve(&record.session);
        // A finished subagent of a live session can still be sent a message: it is idle, not gone.
        let status = if record.status == "completed" && live.contains(session.as_str()) {
            "idle".to_string()
        } else {
            record.status.clone()
        };
        let line = record.tool_use_id.as_deref().and_then(|tool| {
            dispatches
                .entry(session.clone())
                .or_insert_with(|| read_dispatch(state, &session))
                .get(tool)
                .cloned()
        });
        let caller = line
            .as_ref()
            .and_then(|line| line.caller_agent_id.clone())
            .filter(|caller| *caller != record.agent_id && ids.contains(caller));
        let steps = agent_dirs
            .get(&session)
            .map(|dir| {
                super::graph_steps::latest(
                    &dir.join(format!("agent-{}.jsonl", file_safe(&record.agent_id))),
                )
            })
            .unwrap_or_default();
        nodes.insert(
            record.agent_id.clone(),
            Node {
                id: record.agent_id.clone(),
                parent: Some(caller.unwrap_or(session)),
                kind: "subagent".to_string(),
                harness: Some("claude".to_string()),
                model: record.model.clone().or(record.requested_model.clone()),
                effort: None,
                role: Some(record.agent_type.clone()).filter(|t| !t.is_empty()),
                status,
                started_at: Some(record.started_at),
                ended_at: record.ended_at,
                tokens: record
                    .ended_at
                    .map(|_| record.input_tokens.saturating_add(record.output_tokens)),
                label: record
                    .description
                    .as_deref()
                    .map(super::snapshot::redact_text),
                job: record.description.as_deref().and_then(job_text),
                workflow: line.and_then(|line| line.workflow),
                session: None,
                steps,
            },
        );
    }
}

/// A live session's latest steps: a Claude session's main transcript, or the rollout a Codex
/// session's pointer file names.
fn place_session_steps(
    state: &StateDir,
    session_records: &[(sessions::Record, bool)],
    claude_home: Option<&Path>,
    nodes: &mut BTreeMap<String, Node>,
) {
    let mut adapter = super::adapters::claude::ClaudeAdapter::new(None);
    if let Some(home) = claude_home {
        adapter = adapter.with_home(home.to_path_buf());
    }
    for (record, _) in session_records.iter().filter(|(_, alive)| *alive) {
        let path = match record.agent.as_str() {
            "claude" => Some(adapter.transcript_path(&super::event::SessionRef {
                id: super::event::SessionId::parse(&record.session),
                cwd: record.repo.clone(),
            })),
            "codex" => {
                std::fs::read_to_string(state.rollouts().join(format!("{}.path", record.short)))
                    .ok()
                    .map(|pointer| PathBuf::from(pointer.trim()))
            }
            _ => None,
        };
        if let (Some(path), Some(node)) = (path, nodes.get_mut(&record.session)) {
            node.steps = super::graph_steps::latest(&path);
        }
    }
}

/// Dashboard panes and `zirv agent` runs: the launch record gives a worker its parent, task and
/// workflow step; spend rows from before launch records existed place the rest under their
/// parent, with no job where nothing recorded one.
fn place_workers(
    state: &StateDir,
    resolve: &dyn Fn(&str) -> String,
    nodes: &mut BTreeMap<String, Node>,
) {
    let spend: BTreeMap<String, log::DelegationRow> = log::read_delegations(state, SPEND_ROWS)
        .into_iter()
        .filter(|row| !row.session.is_empty() && !row.parent_session.is_empty())
        .map(|row| (row.session.clone(), row))
        .collect();
    let launches: BTreeMap<String, LaunchRecord> = read_launch_records(state)
        .into_iter()
        .map(|record| (record.session.clone(), record))
        .collect();

    for launch in launches.values() {
        if launch.origin != "pane" && !nodes.contains_key(&launch.session) {
            continue;
        }
        let parent = launch.parent_session.as_deref().map(resolve);
        let node = nodes.entry(launch.session.clone()).or_insert_with(|| Node {
            id: launch.session.clone(),
            parent: None,
            kind: "session".to_string(),
            harness: launch.harness.clone(),
            model: None,
            effort: None,
            role: None,
            status: "ended".to_string(),
            started_at: Some(launch.started_at),
            ended_at: None,
            tokens: None,
            label: Some(sessions::short_id(&launch.session)),
            job: None,
            workflow: None,
            session: None,
            steps: Vec::new(),
        });
        // A delegation record owns its node's parent; a pane's launch record supplies it.
        if launch.origin == "pane" || node.parent.is_none() {
            node.parent = parent.or(node.parent.take());
        }
        node.harness = node.harness.take().or(launch.harness.clone());
        node.model = node.model.take().or(launch.model.clone());
        node.job = node.job.take().or(launch.task.clone());
        node.workflow = launch.workflow.clone().or(node.workflow.take());
    }
    for (session, row) in spend {
        let parent = resolve(&row.parent_session);
        if !nodes.contains_key(&parent) {
            continue;
        }
        let tokens = row
            .input_tokens
            .saturating_add(row.cache_creation_input_tokens)
            .saturating_add(row.cache_read_input_tokens)
            .saturating_add(row.output_tokens);
        let node = nodes.entry(session.clone()).or_insert_with(|| Node {
            id: session.clone(),
            parent: None,
            kind: "session".to_string(),
            harness: Some(row.agent.clone()),
            model: None,
            effort: None,
            role: None,
            status: "ended".to_string(),
            started_at: Some(row.ts.saturating_sub(row.wall_ms / 1000)),
            ended_at: Some(row.ts),
            tokens: Some(tokens),
            label: Some(sessions::short_id(&session)),
            job: None,
            workflow: None,
            session: None,
            steps: Vec::new(),
        });
        node.parent = node.parent.take().or(Some(parent));
        node.model = node.model.take().or(row.model.clone());
        if node.status == "ended" {
            node.ended_at = node.ended_at.or(Some(row.ts));
            node.tokens = node.tokens.or(Some(tokens));
        }
    }
}

/// Set every node's `session` to the topmost `session` node above it (itself for a session),
/// or `None` for an orphan whose chain never reaches one.
fn attribute_sessions(nodes: &mut BTreeMap<String, Node>) {
    let roots: BTreeMap<String, Option<String>> = nodes
        .keys()
        .map(|id| {
            let mut top: Option<&Node> = None;
            let mut current = nodes.get(id);
            for _ in 0..32 {
                let Some(node) = current else { break };
                if node.kind == "session" {
                    top = Some(node);
                }
                current = node.parent.as_deref().and_then(|parent| nodes.get(parent));
            }
            (id.clone(), top.map(|node| node.id.clone()))
        })
        .collect();
    for (id, root) in roots {
        if let Some(node) = nodes.get_mut(&id) {
            node.session = root;
        }
    }
}

/// Depth-first `(depth, node)` order. A node whose parent is absent, itself,
/// or part of a cycle is shown as a root, so every node appears exactly once.
pub fn tree_order(nodes: &[Node]) -> Vec<(usize, &Node)> {
    let ids: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut children: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    let mut roots: Vec<&Node> = Vec::new();
    for node in nodes {
        match node.parent.as_deref() {
            Some(parent) if parent != node.id && ids.contains(parent) => {
                children.entry(parent).or_default().push(node)
            }
            _ => roots.push(node),
        }
    }
    let mut out = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    fn walk<'a>(
        node: &'a Node,
        depth: usize,
        children: &BTreeMap<&str, Vec<&'a Node>>,
        seen: &mut BTreeSet<&'a str>,
        out: &mut Vec<(usize, &'a Node)>,
    ) {
        if !seen.insert(node.id.as_str()) {
            return;
        }
        out.push((depth, node));
        for child in children.get(node.id.as_str()).into_iter().flatten() {
            walk(child, depth + 1, children, seen, out);
        }
    }
    for root in roots {
        walk(root, 0, &children, &mut seen, &mut out);
    }
    // Members of a pure cycle have no root above them.
    for node in nodes {
        walk(node, 0, &children, &mut seen, &mut out);
    }
    out
}

// -- Merged event log -------------------------------------------------------

fn jsonl_tail(path: &Path) -> Vec<Value> {
    log::read_jsonl_tail_best_effort::<Value>(path, JSONL_TAIL_BYTES)
}

fn min_f64(values: impl Iterator<Item = f64>) -> Option<f64> {
    values.fold(None, |low, v| Some(low.map_or(v, |l: f64| l.min(v))))
}

fn str_of<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Every source merged into time order (oldest first). A malformed line or
/// unreadable source contributes nothing.
pub fn merged_events(state: &StateDir) -> Vec<Event> {
    merged_events_with_mail(
        state,
        &super::mail::recent_edges(state, now_secs(), EVENTS_PER_SOURCE),
    )
}

/// [`merged_events`] over mail edges the caller already read, so a caller that also needs the
/// edges (the agent tree's unread marks) reads the delivery records once.
pub(super) fn merged_events_with_mail(
    state: &StateDir,
    mail: &[super::mail::MailEdge],
) -> Vec<Event> {
    let mut events = Vec::new();
    for edge in mail {
        // The supervisor mails its advice from a sender named `supervisor`.
        if edge.from_session == SUPERVISOR_SENDER && edge.topic.as_deref() == Some("supervisor") {
            let to = edge
                .to_session
                .as_deref()
                .map_or_else(|| edge.to_label.clone(), sessions::short_id);
            events.push(Event {
                ts: edge.ts,
                actor: SUPERVISOR_SENDER.to_string(),
                kind: "supervisor".to_string(),
                summary: format!("\u{2192} {to}  {}", edge.first_line),
                p: None,
                to: Some(to),
            });
            continue;
        }
        events.push(Event {
            ts: edge.ts,
            actor: edge.from_session.clone(),
            kind: "mail".to_string(),
            summary: edge.first_line.clone(),
            p: None,
            to: Some(edge.to_label.clone()),
        });
    }

    let decisions = log::read_recent_decisions(state);
    for d in decisions.iter().rev().take(EVENTS_PER_SOURCE) {
        events.push(Event {
            ts: d.ts,
            actor: d.session.clone(),
            kind: "decision".to_string(),
            summary: format!("{} {} {}: {}", d.verb, d.verdict, d.action, d.detail),
            p: None,
            to: None,
        });
    }
    for d in log::read_delegations(state, EVENTS_PER_SOURCE) {
        events.push(Event {
            ts: d.ts,
            actor: d.parent_session.clone(),
            kind: "delegation".to_string(),
            summary: format!(
                "{} {} {} ({} in / {} out)",
                d.agent,
                d.model.as_deref().unwrap_or("-"),
                d.outcome,
                d.input_tokens
                    .saturating_add(d.cache_creation_input_tokens)
                    .saturating_add(d.cache_read_input_tokens),
                d.output_tokens
            ),
            p: None,
            to: None,
        });
    }
    let safety = log::read_safety_decisions(state);
    for d in safety.iter().rev().take(EVENTS_PER_SOURCE) {
        events.push(Event {
            ts: d.ts,
            actor: d.session.clone(),
            kind: "safety".to_string(),
            summary: format!("{} {}", d.verdict, d.family),
            p: None,
            to: None,
        });
    }
    for row in jsonl_tail(&state.root().join("jev-decisions.jsonl")) {
        let Some(ts) = row.get("ts").and_then(Value::as_u64) else {
            continue;
        };
        let p = row
            .get("answers")
            .and_then(Value::as_object)
            .and_then(|answers| {
                min_f64(
                    answers
                        .values()
                        .filter_map(|a| a.get("margin").and_then(Value::as_f64)),
                )
            });
        events.push(Event {
            ts,
            actor: str_of(&row, "session").to_string(),
            kind: "jev".to_string(),
            summary: format!(
                "{}{}",
                str_of(&row, "site"),
                if row.get("cached").and_then(Value::as_bool) == Some(true) {
                    " (cached)"
                } else {
                    ""
                }
            ),
            p,
            to: None,
        });
    }
    for row in jsonl_tail(&state.root().join("proxy-decisions.jsonl")) {
        let Some(ts) = row.get("created_at").and_then(Value::as_u64) else {
            continue;
        };
        let word = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string()
        };
        let p = row
            .get("confidence")
            .and_then(Value::as_object)
            .and_then(|c| min_f64(c.values().filter_map(Value::as_f64)));
        events.push(Event {
            ts,
            actor: "proxy".to_string(),
            kind: "proxy".to_string(),
            summary: format!(
                "intent={} complexity={} execution={} decider={}",
                word("intent"),
                word("complexity"),
                word("execution"),
                word("decider")
            ),
            p,
            to: None,
        });
    }
    for record in read_subagent_records(state) {
        let label = format!("{} {}", record.agent_type, record.agent_id);
        events.push(Event {
            ts: record.started_at,
            actor: record.session.clone(),
            kind: "subagent_start".to_string(),
            summary: label.clone(),
            p: None,
            to: Some(record.agent_id.clone()),
        });
        if let Some(ended) = record.ended_at {
            events.push(Event {
                ts: ended,
                actor: record.session.clone(),
                kind: "subagent_stop".to_string(),
                summary: format!(
                    "{label} {} ({} tokens)",
                    record.status,
                    record.input_tokens.saturating_add(record.output_tokens)
                ),
                p: None,
                to: None,
            });
        }
    }
    events.sort_by_key(|event| event.ts);
    events
}

// -- CLI --------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct GraphArgs {
    /// Print the nodes (and events) as JSON instead of an indented tree.
    #[arg(long)]
    pub json: bool,
    /// Also print the newest N merged events (`--json` prints the newest 100 when omitted).
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub events: usize,
    /// Only the agents under this session (a full id or its short id).
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
    /// Only the agents under sessions registered for the current repository.
    #[arg(long)]
    pub repo: bool,
}

/// Events printed by `--json` when `--events` is not given.
const JSON_EVENTS: usize = 100;

fn node_line(node: &Node) -> String {
    let name = node.label.as_deref().unwrap_or(&node.id);
    let mut parts = vec![format!("{name} [{}]", node.kind), node.status.clone()];
    parts.extend(node.harness.clone());
    parts.extend(node.model.clone());
    parts.extend(node.effort.clone());
    parts.extend(node.role.clone());
    parts.extend(node.tokens.map(|t| format!("{t} tok")));
    parts.extend(
        node.workflow
            .as_ref()
            .map(|w| format!("{}:{}", w.pack, w.step)),
    );
    parts.extend(node.job.as_ref().map(|job| format!("- {job}")));
    parts.join(" ")
}

/// Keep the nodes under the requested session or repository, and the events that name one of
/// them as actor or recipient.
fn scope_to(
    nodes: Vec<Node>,
    events: Vec<Event>,
    session: Option<&str>,
    repo_sessions: Option<&BTreeSet<String>>,
) -> (Vec<Node>, Vec<Event>) {
    let wanted = |root: &str| {
        session.is_none_or(|id| root.starts_with(id) || sessions::short_id(root) == id)
            && repo_sessions.is_none_or(|ids| ids.contains(root))
    };
    let nodes: Vec<Node> = nodes
        .into_iter()
        .filter(|node| node.session.as_deref().is_some_and(wanted))
        .collect();
    let shorts: BTreeSet<String> = nodes
        .iter()
        .map(|node| sessions::short_id(&node.id))
        .collect();
    let events = events
        .into_iter()
        .filter(|event| {
            shorts.contains(&sessions::short_id(&event.actor))
                || event
                    .to
                    .as_deref()
                    .is_some_and(|to| shorts.contains(&sessions::short_id(to)))
        })
        .collect();
    (nodes, events)
}

pub fn run<W: Write>(args: &GraphArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    let state = StateDir::resolve(&env)?;
    let repo = std::env::current_dir()?;
    let codex_root = env("HOME").map(|home| PathBuf::from(home).join(".codex/sessions"));
    let nodes = snapshot(&state, &repo, codex_root.as_deref(), now_secs());
    let wanted_events = match (args.events, args.json) {
        (0, true) => JSON_EVENTS,
        (count, _) => count,
    };
    let events = if wanted_events > 0 {
        let all = merged_events(&state);
        all[all.len().saturating_sub(wanted_events)..].to_vec()
    } else {
        Vec::new()
    };
    let (nodes, events) = if args.session.is_some() || args.repo {
        let slug = super::state::repo_slug_read_only(&repo);
        let repo_sessions: BTreeSet<String> = read_session_records(&state)
            .into_iter()
            .filter(|(record, _)| record.repo_slug == slug)
            .map(|(record, _)| record.session)
            .collect();
        scope_to(
            nodes,
            events,
            args.session.as_deref(),
            args.repo.then_some(&repo_sessions),
        )
    } else {
        (nodes, events)
    };
    if args.json {
        let doc =
            serde_json::json!({"schema_version": SCHEMA_VERSION, "nodes": nodes, "events": events});
        writeln!(w, "{}", serde_json::to_string_pretty(&doc)?)?;
        return Ok(0);
    }
    if nodes.is_empty() {
        writeln!(w, "no agents")?;
    }
    for (depth, node) in tree_order(&nodes) {
        writeln!(w, "{}{}", "  ".repeat(depth), node_line(node))?;
    }
    if args.events > 0 {
        writeln!(w, "events:")?;
        for event in &events {
            let p = event.p.map(|p| format!(" p={p:.2}")).unwrap_or_default();
            writeln!(
                w,
                "  {} {} {} {}{p}",
                event.ts, event.actor, event.kind, event.summary
            )?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_for(dir: &Path) -> std::collections::HashMap<String, String> {
        [(
            super::super::state::STATE_ENV.to_string(),
            dir.join("state").display().to_string(),
        )]
        .into()
    }

    fn start_payload(agent: &str, transcript: &Path) -> String {
        serde_json::json!({
            "session_id": "sess-832-abcdef",
            "agent_id": agent,
            "agent_type": "general-purpose",
            "transcript_path": transcript.display().to_string(),
        })
        .to_string()
    }

    fn state_for(dir: &Path) -> StateDir {
        StateDir::from_path(dir.join("state"))
    }

    #[test]
    fn subagent_start_creates_a_node_and_prints_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_for(dir.path());
        let lead = dir.path().join("lead.jsonl");
        let code = run_subagent_start(&start_payload("agent-a1", &lead), &|k| env.get(k).cloned())
            .expect("never errors");
        assert_eq!(code, 0);
        let nodes = snapshot(&state_for(dir.path()), dir.path(), None, now_secs());
        let node = nodes.iter().find(|n| n.id == "agent-a1").expect("node");
        assert_eq!(node.status, "running");
        assert_eq!(node.parent.as_deref(), Some("sess-832-abcdef"));
        assert_eq!(node.role.as_deref(), Some("general-purpose"));
    }

    #[test]
    fn subagent_start_with_bad_payload_is_a_silent_passthrough() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_for(dir.path());
        for raw in ["", "not json", "{}", r#"{"session_id":"s"}"#] {
            assert_eq!(
                run_subagent_start(raw, &|k| env.get(k).cloned()).expect("ok"),
                0
            );
        }
        assert!(!graph_root(&state_for(dir.path())).exists());
    }

    #[test]
    fn subagent_stop_closes_the_node_with_model_tokens_and_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_for(dir.path());
        let lead = dir.path().join("lead.jsonl");
        let sub_dir = dir.path().join("lead").join("subagents");
        std::fs::create_dir_all(&sub_dir).expect("mkdir");
        let transcript = sub_dir.join("agent-a2.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":"hi"}}"#,
                "\n",
                r#"{"type":"assistant","isSidechain":true,"requestId":"r1","message":{"id":"m1","model":"claude-haiku-4-5","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":90,"output_tokens":7}}}"#,
                "\n",
            ),
        )
        .expect("transcript");
        std::fs::write(
            sub_dir.join("agent-a2.meta.json"),
            r#"{"agentType":"general-purpose","description":"do it","model":"haiku","toolUseId":"toolu_meta"}"#,
        )
        .expect("meta");
        let lookup = |k: &str| env.get(k).cloned();
        run_subagent_start(&start_payload("a2", &lead), &lookup).expect("start");
        record_subagent_stop(&start_payload("a2", &lead), &lookup);
        let nodes = snapshot(&state_for(dir.path()), dir.path(), None, now_secs());
        let node = nodes.iter().find(|n| n.id == "a2").expect("node");
        assert_eq!(node.status, "completed");
        assert!(node.ended_at.is_some());
        assert_eq!(node.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(node.tokens, Some(107));
        assert_eq!(node.label.as_deref(), Some("do it"));
        assert_eq!(node.job.as_deref(), Some("do it"));
        assert_eq!(node.parent.as_deref(), Some("sess-832-abcdef"));
        let record = read_subagent_records(&state_for(dir.path()))
            .pop()
            .expect("record");
        assert_eq!(record.tool_use_id.as_deref(), Some("toolu_meta"));
    }

    #[test]
    fn nested_parent_links_resolve_to_any_depth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_for(dir.path());
        let state = state_for(dir.path());
        // a is dispatched by the session, b by a, c by b.
        for (tool, caller) in [("t-a", ""), ("t-b", "a"), ("t-c", "b")] {
            record_agent_dispatch(
                &|k| env.get(k).cloned(),
                "sess-832-abcdef",
                tool,
                caller,
                None,
            );
        }
        for id in ["a", "b", "c"] {
            save(
                &state,
                &SubagentRecord {
                    session: "sess-832-abcdef".to_string(),
                    agent_id: id.to_string(),
                    agent_type: "t".to_string(),
                    tool_use_id: Some(format!("t-{id}")),
                    status: "running".to_string(),
                    started_at: 1,
                    ..SubagentRecord::default()
                },
            );
        }
        let nodes = snapshot(&state, dir.path(), None, now_secs());
        let order: Vec<(usize, &str)> = tree_order(&nodes)
            .into_iter()
            .map(|(depth, node)| (depth, node.id.as_str()))
            .collect();
        // "a" hangs off a session that is not itself a node, so it is a root.
        assert_eq!(order, vec![(0, "a"), (1, "b"), (2, "c")]);
        assert!(
            nodes.iter().all(|n| n.session.is_none()),
            "no session node, so no attribution"
        );
    }

    #[test]
    fn tree_order_survives_a_parent_cycle() {
        let make = |id: &str, parent: &str| Node {
            id: id.to_string(),
            parent: Some(parent.to_string()),
            kind: "subagent".to_string(),
            harness: None,
            model: None,
            effort: None,
            role: None,
            status: "running".to_string(),
            started_at: None,
            ended_at: None,
            tokens: None,
            label: None,
            job: None,
            workflow: None,
            session: None,
            steps: Vec::new(),
        };
        let nodes = vec![make("x", "y"), make("y", "x")];
        assert_eq!(tree_order(&nodes).len(), 2);
    }

    #[test]
    fn merged_events_order_by_timestamp_across_sources_and_skip_malformed_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        std::fs::create_dir_all(state.root()).expect("mkdir");
        std::fs::write(
            state.root().join("jev-decisions.jsonl"),
            "{\"ts\":30,\"site\":\"intake\",\"session\":\"s1\",\"answers\":{\"a\":{\"margin\":0.9},\"b\":{\"margin\":0.4}}}\nnot json\n{\"no_ts\":true}\n",
        )
        .expect("jev");
        std::fs::write(
            state.root().join("proxy-decisions.jsonl"),
            "{\"created_at\":10,\"intent\":\"implement\",\"complexity\":\"bounded\",\"execution\":\"delegate\",\"decider\":\"rules\",\"confidence\":{\"x\":0.8,\"y\":0.6}}\n{broken\n",
        )
        .expect("proxy");
        std::fs::create_dir_all(state.logs()).expect("logs");
        std::fs::write(
            state.logs().join("decisions.jsonl"),
            "{\"ts\":20,\"session\":\"s1\",\"verb\":\"wrap\",\"verdict\":\"ok\",\"score\":1,\"action\":\"none\",\"detail\":\"d\"}\n}{\n",
        )
        .expect("decisions");
        save(
            &state,
            &SubagentRecord {
                session: "s1".to_string(),
                agent_id: "ag".to_string(),
                agent_type: "Explore".to_string(),
                status: "completed".to_string(),
                started_at: 15,
                ended_at: Some(40),
                ..SubagentRecord::default()
            },
        );
        let events = merged_events(&state);
        let order: Vec<(u64, &str)> = events.iter().map(|e| (e.ts, e.kind.as_str())).collect();
        assert_eq!(
            order,
            vec![
                (10, "proxy"),
                (15, "subagent_start"),
                (20, "decision"),
                (30, "jev"),
                (40, "subagent_stop")
            ]
        );
        assert_eq!(events[0].p, Some(0.6));
        assert_eq!(events[3].p, Some(0.4));
    }

    #[test]
    fn node_directories_are_pruned_with_their_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let orphan = graph_root(&state).join("deadbeef");
        std::fs::create_dir_all(&orphan).expect("mkdir");
        std::fs::write(orphan.join("x.json"), "{}").expect("node");
        let fresh = graph_root(&state).join("cafef00d");
        std::fs::create_dir_all(&fresh).expect("mkdir");
        // Idle past the window relative to a far-future "now": only the
        // unregistered directory is dropped when its session is registered.
        let record = sessions::Record {
            session: "cafef00d-0000".to_string(),
            short: "cafef00d".to_string(),
            agent: "claude".to_string(),
            repo: dir.path().to_path_buf(),
            repo_slug: "r".to_string(),
            verb: sessions::Verb::Wrap,
            pid: 1,
            started_at: 1,
            reachable: true,
            owner_pid: None,
            safety_policy_sha256: None,
            role: None,
            start_time: None,
            in_flight: None,
            runtime: Default::default(),
        };
        std::fs::create_dir_all(state.sessions()).expect("sessions");
        std::fs::write(
            state.sessions().join("cafef00d.json"),
            serde_json::to_string(&record).expect("json"),
        )
        .expect("record");
        prune(&state, now_secs() + NODE_DIR_MAX_IDLE_SECS + 10);
        assert!(!orphan.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn prune_runs_at_most_once_per_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let session = graph_root(&state).join("aaaa1111");
        create_private_dir_all(&session).expect("session dir");
        for i in 0..KEEP_NODES_PER_SESSION + 5 {
            std::fs::write(session.join(format!("{i}.json")), "{}").expect("node");
        }
        let count = || std::fs::read_dir(&session).expect("dir").count();
        prune_if_due(&state, now_secs());
        assert_eq!(count(), KEEP_NODES_PER_SESSION, "first call prunes");
        std::fs::write(session.join("extra.json"), "{}").expect("extra");
        prune_if_due(&state, now_secs());
        assert_eq!(
            count(),
            KEEP_NODES_PER_SESSION + 1,
            "second call is skipped"
        );
        prune_if_due(&state, now_secs() + PRUNE_INTERVAL_SECS + 1);
        assert_eq!(
            count(),
            KEEP_NODES_PER_SESSION,
            "due again after the interval"
        );
    }

    #[test]
    fn an_unchanged_rollout_is_not_parsed_again_and_a_changed_one_is() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = dir.path().join("2026/10/01");
        std::fs::create_dir_all(&day).expect("day dir");
        let file = day.join("rollout-a.jsonl");
        let fixture = include_str!("../../../tests/fixtures/codex-subagent-rollout.jsonl");
        std::fs::write(&file, fixture).expect("write");
        let now = now_secs();
        let parses = || ROLLOUT_PARSES.load(std::sync::atomic::Ordering::SeqCst);

        assert_eq!(codex_children(dir.path(), now).len(), 1);
        let first = parses();
        assert_eq!(codex_children(dir.path(), now).len(), 1);
        assert_eq!(parses(), first, "unchanged file is served from the cache");

        std::fs::write(&file, format!("{fixture}\n")).expect("grow");
        assert_eq!(codex_children(dir.path(), now).len(), 1);
        assert_eq!(parses(), first + 1, "a changed file is parsed again");
    }

    #[test]
    fn an_appended_rollout_folds_only_new_bytes_and_matches_a_full_parse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = dir.path().join("2026/10/01");
        std::fs::create_dir_all(&day).expect("day dir");
        let file = day.join("rollout-inc.jsonl");
        let fixture = include_str!("../../../tests/fixtures/codex-subagent-rollout.jsonl");
        let now = now_secs();
        let one = |text: &str| {
            codex_children(dir.path(), now)
                .into_iter()
                .next()
                .map(|n| (text.len(), n))
        };
        let mut cuts: Vec<usize> = fixture.match_indices('\n').map(|(i, _)| i + 1).collect();
        // Mid-line cuts too, so a partial trailing line is exercised.
        cuts.extend(
            fixture
                .match_indices('\n')
                .map(|(i, _)| i.saturating_sub(7)),
        );
        cuts.sort_unstable();
        cuts.push(fixture.len());
        for cut in cuts {
            if !fixture.is_char_boundary(cut) {
                continue;
            }
            let text = &fixture[..cut];
            std::fs::write(&file, text).expect("write");
            // Same length would hit the cache; force a distinct mtime/len key per step.
            let got = one(text).map(|(_, n)| n);
            assert_eq!(got, parse_codex_child(text), "cut at byte {cut}");
        }
        // A shorter replacement restarts from byte 0.
        let short = fixture.lines().next().expect("head").to_string() + "\n";
        std::fs::write(&file, &short).expect("shrink");
        assert_eq!(one(&short).map(|(_, n)| n), parse_codex_child(&short));
    }

    #[test]
    fn codex_child_rollout_parses_into_a_node_and_skips_guardian_threads() {
        let fixture = include_str!("../../../tests/fixtures/codex-subagent-rollout.jsonl");
        let node = parse_codex_child(fixture).expect("child");
        assert_eq!(node.id, "01a0f65b-2d47-7b30-b693-bbc402b90dc5");
        assert_eq!(
            node.parent.as_deref(),
            Some("01a0f65a-9111-71d3-a061-354ed3c9f85d")
        );
        assert_eq!(node.harness.as_deref(), Some("codex"));
        assert_eq!(node.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(node.effort.as_deref(), Some("high"));
        assert_eq!(node.role.as_deref(), Some("frontend_sol"));
        assert_eq!(node.status, "completed");
        assert_eq!(node.tokens, Some(1050));
        assert_eq!(
            node.ended_at.zip(node.started_at).map(|(e, s)| e - s),
            Some(1435)
        );

        let guardian = include_str!("../../../tests/fixtures/codex-guardian-rollout.jsonl");
        assert!(parse_codex_child(guardian).is_none());
    }

    #[test]
    fn a_running_hook_record_takes_the_native_status_once_the_agent_is_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let session = "aaaa1111-0000-4000-8000-000000000001";
        register_session(&state, session, "claude", &repo);
        let subagents = home
            .join(".claude/projects")
            .join(super::super::adapters::claude::project_slug(&repo))
            .join(session)
            .join("subagents");
        let secret = "token = AKIAIOSFODNN7EXAMPLE0123456789abcdefghijklmnop";
        write_native(
            &subagents,
            "gone1",
            &format!(r#"{{"agentType":"Explore","description":"{secret}","toolUseId":"toolu_1"}}"#),
            &[
                r#"{"type":"user","timestamp":"2026-10-01T10:00:00.000Z","message":{"content":"go"}}"#,
                r#"{"type":"user","timestamp":"2026-10-01T10:01:00.000Z","toolEndsTurn":true}"#,
            ],
        );
        save(
            &state,
            &SubagentRecord {
                session: session.to_string(),
                agent_id: "gone1".to_string(),
                agent_type: "Explore".to_string(),
                status: "running".to_string(),
                started_at: 7,
                ..SubagentRecord::default()
            },
        );
        let nodes = snapshot_in(&state, &repo, None, Some(&home), now_secs());
        let node = nodes.iter().find(|n| n.id == "gone1").expect("node");
        assert_eq!(node.status, "idle");
        assert!(node.ended_at.is_some());
        let label = node.label.as_deref().unwrap_or_default();
        assert!(!label.contains("AKIAIOSFODNN7"), "{label}");
    }

    #[test]
    fn mail_edges_join_the_merged_log_as_mail_events_addressed_to_their_recipient() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        std::fs::create_dir_all(state.root()).expect("mkdir");
        let edges = vec![super::super::mail::MailEdge {
            ts: 25,
            from_session: "aaaaaaaa".into(),
            to_session: Some("bbbbbbbb".into()),
            to_label: "bbbbbbbb".into(),
            unread: true,
            first_line: "run the tests".into(),
            topic: None,
        }];
        std::fs::write(
            state.root().join("jev-decisions.jsonl"),
            "{\"ts\":30,\"site\":\"intake\",\"session\":\"s1\"}\n",
        )
        .expect("jev");
        let events = merged_events_with_mail(&state, &edges);
        let order: Vec<(u64, &str)> = events.iter().map(|e| (e.ts, e.kind.as_str())).collect();
        assert_eq!(order, vec![(25, "mail"), (30, "jev")]);
        assert_eq!(events[0].actor, "aaaaaaaa");
        assert_eq!(events[0].to.as_deref(), Some("bbbbbbbb"));
        assert_eq!(events[0].summary, "run the tests");
        let json = serde_json::to_string(&events).expect("json");
        assert!(json.contains("\"to\":\"bbbbbbbb\""));
        assert_eq!(
            json.matches("\"to\"").count(),
            1,
            "non-mail events keep their old shape"
        );
    }

    /// A live session (this test process) registered for `repo`.
    fn register_session(state: &StateDir, session: &str, agent: &str, repo: &Path) {
        let mut record = sessions::Record::new(session, agent, repo, sessions::Verb::Wrap);
        record.pid = std::process::id();
        std::fs::create_dir_all(state.sessions()).expect("sessions");
        std::fs::write(
            state.sessions().join(format!("{}.json", record.short)),
            serde_json::to_string(&record).expect("json"),
        )
        .expect("record");
    }

    fn write_native(dir: &Path, id: &str, meta: &str, rows: &[&str]) {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join(format!("agent-{id}.meta.json")), meta).expect("meta");
        std::fs::write(
            dir.join(format!("agent-{id}.jsonl")),
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .expect("transcript");
    }

    #[test]
    fn meta_files_without_hook_records_become_subagent_nodes_and_a_hook_record_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let session = "aaaa1111-0000-4000-8000-000000000001";
        register_session(&state, session, "claude", &repo);
        let subagents = home
            .join(".claude/projects")
            .join(super::super::adapters::claude::project_slug(&repo))
            .join(session)
            .join("subagents");
        write_native(
            &subagents,
            "done1",
            r#"{"agentType":"Explore","description":"Map the call sites","toolUseId":"toolu_1","model":"haiku"}"#,
            &[
                r#"{"type":"user","timestamp":"2026-10-01T10:00:00.000Z","message":{"content":"go"}}"#,
                r#"{"type":"assistant","timestamp":"2026-10-01T10:00:30.000Z","requestId":"r1","message":{"id":"m1","model":"claude-haiku-4-5","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":5}}}"#,
                r#"{"type":"user","timestamp":"2026-10-01T10:01:00.000Z","toolEndsTurn":true}"#,
            ],
        );
        write_native(
            &subagents,
            "busy2",
            r#"{"agentType":"general-purpose","description":"Still working on it","toolUseId":"toolu_2"}"#,
            &[
                r#"{"type":"user","timestamp":"2026-10-01T10:05:00.000Z","message":{"content":"go"}}"#,
                r#"{"type":"assistant","timestamp":"2026-10-01T10:05:10.000Z","message":{"stop_reason":null,"content":[{"type":"tool_use"}]}}"#,
            ],
        );
        write_native(
            &subagents,
            "hook3",
            r#"{"agentType":"Plan","description":"from meta","toolUseId":"toolu_3"}"#,
            &[r#"{"type":"user","timestamp":"2026-10-01T10:06:00.000Z"}"#],
        );
        save(
            &state,
            &SubagentRecord {
                session: session.to_string(),
                agent_id: "hook3".to_string(),
                agent_type: "Plan".to_string(),
                description: Some("from hook".to_string()),
                status: "completed".to_string(),
                started_at: 7,
                ..SubagentRecord::default()
            },
        );

        let nodes = snapshot_in(&state, &repo, None, Some(&home), now_secs());
        let node = |id: &str| nodes.iter().find(|n| n.id == id).expect(id);
        let done = node("done1");
        assert_eq!(done.kind, "subagent");
        assert_eq!(done.status, "idle");
        assert_eq!(done.parent.as_deref(), Some(session));
        assert_eq!(done.session.as_deref(), Some(session));
        assert_eq!(done.job.as_deref(), Some("Map the call sites"));
        assert_eq!(done.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(done.tokens, Some(15));
        assert_eq!(
            done.ended_at
                .zip(done.started_at)
                .map(|(end, start)| end - start),
            Some(60)
        );
        let busy = node("busy2");
        assert_eq!((busy.status.as_str(), busy.ended_at), ("running", None));
        assert_eq!(busy.job.as_deref(), Some("Still working on it"));
        let hooked = node("hook3");
        assert_eq!(hooked.job.as_deref(), Some("from hook"));
        assert_eq!(hooked.status, "idle");
        assert_eq!(hooked.started_at, Some(7));
    }

    #[test]
    fn a_finished_native_agent_is_idle_under_a_live_session_and_completed_under_a_dead_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let live = "aaaa1111-0000-4000-8000-000000000001";
        let dead = "bbbb2222-0000-4000-8000-000000000002";
        register_session(&state, live, "claude", &repo);
        register_session(&state, dead, "claude", &repo);
        let mut gone = std::process::Command::new("true").spawn().expect("spawn");
        gone.wait().expect("wait");
        let path = state.sessions().join(format!("{}.json", &dead[..8]));
        let mut record: sessions::Record =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        record.pid = gone.id();
        std::fs::write(&path, serde_json::to_string(&record).expect("json")).expect("write");
        for (session, id) in [(live, "live1"), (dead, "dead1")] {
            let subagents = home
                .join(".claude/projects")
                .join(super::super::adapters::claude::project_slug(&repo))
                .join(session)
                .join("subagents");
            write_native(
                &subagents,
                id,
                r#"{"agentType":"Explore","description":"d"}"#,
                &[
                    r#"{"type":"user","timestamp":"2026-10-01T10:00:00.000Z","message":{"content":"go"}}"#,
                    r#"{"type":"user","timestamp":"2026-10-01T10:01:00.000Z","toolEndsTurn":true}"#,
                ],
            );
        }
        let nodes = snapshot_in(&state, &repo, None, Some(&home), now_secs());
        let status = |id: &str| nodes.iter().find(|n| n.id == id).expect(id).status.clone();
        assert_eq!(status("live1"), "idle");
        assert_eq!(status("dead1"), "completed");
    }

    #[test]
    fn a_running_native_agent_of_a_dead_session_reads_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let subagents = dir.path().join("subagents");
        write_native(
            &subagents,
            "gone1",
            r#"{"agentType":"t","description":"d"}"#,
            &[r#"{"type":"user","timestamp":"2026-10-01T10:00:00.000Z"}"#],
        );
        let alive = native_subagents(&subagents, "s", true);
        let dead = native_subagents(&subagents, "s", false);
        assert_eq!(alive[0].status, "running");
        assert_eq!(dead[0].status, "stopped");
    }

    #[test]
    fn dispatch_lines_carry_the_caller_and_the_workflow_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_for(dir.path());
        let state = state_for(dir.path());
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let lookup = |k: &str| env.get(k).cloned();
        record_agent_dispatch(&lookup, "sess-w", "t-plain", "", Some(&repo));
        record_agent_dispatch(&lookup, "sess-w", "", "x", Some(&repo));
        let wf = crate::commands::workflow::engine::WorkflowState::start(
            repo.clone(),
            "synthetic".into(),
            crate::commands::workflow::engine::WorkflowKind::Feature,
            None,
            true,
            crate::commands::workflow::classify::Classification {
                intent: crate::commands::workflow::classify::Intent::Feature,
                complexity: crate::commands::workflow::classify::Complexity::Bounded,
                risk: crate::commands::workflow::classify::RiskBand::Low,
                risk_score: 0,
                changed_files: 1,
                changed_lines: 10,
                changed_paths: Vec::new(),
                declared_scope: true,
                work_domain: Default::default(),
                risk_measurement: Default::default(),
                reasons: Vec::new(),
            },
        );
        crate::commands::workflow::engine::save(&state, &wf, true).expect("save");
        record_agent_dispatch(&lookup, "sess-w", "t-flow", "caller-1", Some(&repo));

        let lines = read_dispatch(&state, "sess-w");
        assert_eq!(lines.len(), 2, "a call without an id leaves no line");
        assert_eq!(lines["t-plain"].workflow, None);
        let stamped = &lines["t-flow"];
        assert_eq!(stamped.caller_agent_id.as_deref(), Some("caller-1"));
        let stamp = stamped.workflow.as_ref().expect("stamp");
        assert_eq!(stamp.id, wf.id);
        assert_eq!(stamp.step, wf.current().expect("step").id);

        // The stamp reaches the node of the agent that call started, and a live session in the
        // repo is on the same step.
        register_session(
            &state,
            "bbbb2222-0000-4000-8000-000000000002",
            "claude",
            &repo,
        );
        save(
            &state,
            &SubagentRecord {
                session: "sess-w".to_string(),
                agent_id: "ag".to_string(),
                tool_use_id: Some("t-flow".to_string()),
                status: "running".to_string(),
                started_at: 1,
                ..SubagentRecord::default()
            },
        );
        let nodes = snapshot_in(&state, &repo, None, Some(dir.path()), now_secs());
        let agent = nodes.iter().find(|n| n.id == "ag").expect("agent");
        assert_eq!(agent.workflow.as_ref().map(|w| &w.id), Some(&wf.id));
        let seat = nodes.iter().find(|n| n.kind == "session").expect("session");
        assert_eq!(seat.workflow.as_ref().map(|w| &w.id), Some(&wf.id));
    }

    #[test]
    fn a_pane_launch_record_outlives_its_swept_session_and_old_spend_rows_still_place_panes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let repo = dir.path().join("repo");
        let seat = "cccc3333-0000-4000-8000-000000000003";
        let pane = "dddd4444-0000-4000-8000-000000000004";
        register_session(&state, seat, "claude", &repo);
        register_session(&state, pane, "codex", &repo);
        record_worker_launch(
            &state,
            &repo,
            &Launch {
                session: pane,
                origin: "pane",
                parent_session: Some("cccc3333"),
                harness: Some("codex"),
                model: Some("gpt-6.1-sol"),
                task: Some("Implement the retry flag\nwith tests and docs"),
                workdir: Some(&repo),
            },
            100,
        );
        // A pane from before launch records: only its spend row remains.
        let legacy = "eeee5555-0000-4000-8000-000000000005";
        std::fs::create_dir_all(state.logs()).expect("logs");
        std::fs::write(
            state.logs().join(log::DELEGATION_FILE),
            format!(
                "{{\"ts\":500,\"session\":\"{legacy}\",\"parent_session\":\"cccc3333\",\"work_group_id\":null,\"agent\":\"codex\",\"model\":\"gpt-6.1-sol\",\"input_tokens\":10,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":5,\"wall_ms\":60000,\"exit_code\":0,\"outcome\":\"ok\"}}\n\
                 {{\"ts\":501,\"session\":\"{seat}\",\"parent_session\":\"\",\"agent\":\"typesafe\",\"model\":null,\"input_tokens\":0,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":0,\"wall_ms\":0,\"exit_code\":0,\"outcome\":\"ok\"}}\n"
            ),
        )
        .expect("spend rows");

        let check = |nodes: &[Node]| {
            let live = nodes.iter().find(|n| n.id == pane).expect("pane node");
            assert_eq!(live.parent.as_deref(), Some(seat));
            assert_eq!(live.session.as_deref(), Some(seat));
            assert_eq!(live.job.as_deref(), Some("Implement the retry flag"));
            assert_eq!(live.model.as_deref(), Some("gpt-6.1-sol"));
            let old = nodes.iter().find(|n| n.id == legacy).expect("legacy pane");
            assert_eq!(old.parent.as_deref(), Some(seat));
            assert_eq!(old.job, None, "no record, so no job");
            assert_eq!(old.tokens, Some(15));
            assert_eq!(old.started_at, Some(440));
            assert_eq!(nodes.iter().filter(|n| n.kind == "session").count(), 3);
        };
        check(&snapshot(&state, &repo, None, now_secs()));

        // The dashboard sweeps the pane's session record when it ends.
        std::fs::remove_file(state.sessions().join("dddd4444.json")).expect("sweep");
        let swept = snapshot(&state, &repo, None, now_secs());
        check(&swept);
        let pane_node = swept.iter().find(|n| n.id == pane).expect("pane");
        assert_eq!(pane_node.status, "ended");
    }

    #[test]
    fn a_delegation_job_is_the_objective_else_the_task_and_is_redacted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let launch = |name: &str, objective: Option<&str>, task: Option<&str>| {
            delegation::record_launch(
                &state,
                &repo,
                delegation::WorkerHandle {
                    delegation: name.to_string(),
                    attempt: 1,
                    runtime: Default::default(),
                    worker_session: format!("{name}-session"),
                    short: name.to_string(),
                    role: "worker".to_string(),
                    task: task.map(str::to_string),
                    group: None,
                    objective: objective.map(str::to_string),
                    workdir: repo.clone(),
                    manifest: None,
                    plan_override: false,
                },
                None,
                10,
            )
            .expect("launch");
        };
        launch("withobj", Some("Ship the objective"), Some("the task"));
        launch("tasklong", None, Some(&"word ".repeat(40)));
        launch("nothing", None, None);
        let nodes = snapshot(&state, &repo, None, now_secs());
        let job = |id: &str| nodes.iter().find(|n| n.id == id).expect(id).job.clone();
        assert_eq!(
            job("withobj-session").as_deref(),
            Some("Ship the objective")
        );
        let long = job("tasklong-session").expect("task job");
        assert_eq!(long.chars().count(), JOB_CHARS);
        assert!(long.ends_with('\u{2026}'));
        assert_eq!(job("nothing-session"), None);
        let secret = "token = AKIAIOSFODNN7EXAMPLE0123456789abcdefghijklmnop";
        let redacted = job_text(secret).expect("job");
        assert!(!redacted.contains("AKIAIOSFODNN7"), "{redacted}");
    }

    #[test]
    fn a_codex_child_job_is_its_first_task_line_and_not_injected_context() {
        let fixture = include_str!("../../../tests/fixtures/codex-subagent-user-task.jsonl");
        let node = parse_codex_child(fixture).expect("child");
        assert_eq!(node.job.as_deref(), Some("Add the synthetic retry flag"));
        // A child whose task is not in the clear has no job.
        let encrypted = include_str!("../../../tests/fixtures/codex-subagent-rollout.jsonl");
        assert_eq!(parse_codex_child(encrypted).expect("child").job, None);
    }

    #[test]
    fn codex_children_are_attributed_to_their_root_session_and_foreign_ones_are_scoped_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        let repo = dir.path().join("repo");
        let mine = "ffff6666-0000-4000-8000-000000000006";
        let other = "9999a777-0000-4000-8000-000000000007";
        register_session(&state, mine, "codex", &repo);
        register_session(&state, other, "codex", &repo);
        let rollouts = dir.path().join("codex/2026/10/01");
        std::fs::create_dir_all(&rollouts).expect("rollouts");
        std::fs::create_dir_all(state.rollouts()).expect("pointer dir");
        let fixture = include_str!("../../../tests/fixtures/codex-subagent-user-task.jsonl");
        let (own_thread, other_thread, stray_thread) = (
            "0aaaaaaa-0000-7000-8000-000000000001",
            "0bbbbbbb-0000-7000-8000-0000000000b1",
            "0ccccccc-0000-7000-8000-0000000000c1",
        );
        // One child per parent thread: mine, another session's, and one nobody owns.
        for (parent, child, owner) in [
            (
                own_thread,
                "0aaaaaaa-0000-7000-8000-000000000002",
                Some(mine),
            ),
            (
                other_thread,
                "0bbbbbbb-0000-7000-8000-0000000000b2",
                Some(other),
            ),
            (stray_thread, "0ccccccc-0000-7000-8000-0000000000c2", None),
        ] {
            let text = fixture
                .replace(own_thread, parent)
                .replace("0aaaaaaa-0000-7000-8000-000000000002", child);
            std::fs::write(rollouts.join(format!("rollout-child-{child}.jsonl")), text)
                .expect("child rollout");
            if let Some(owner) = owner {
                std::fs::write(
                    state
                        .rollouts()
                        .join(format!("{}.path", sessions::short_id(owner))),
                    format!("/x/rollout-2026-10-01T08-00-00-{parent}.jsonl"),
                )
                .expect("pointer");
            }
        }
        let nodes = snapshot(&state, &repo, Some(&dir.path().join("codex")), now_secs());
        let child = |id: &str| nodes.iter().find(|n| n.id == id).expect(id);
        assert_eq!(
            child("0aaaaaaa-0000-7000-8000-000000000002")
                .session
                .as_deref(),
            Some(mine)
        );
        assert_eq!(
            child("0bbbbbbb-0000-7000-8000-0000000000b2")
                .session
                .as_deref(),
            Some(other)
        );
        assert_eq!(child("0ccccccc-0000-7000-8000-0000000000c2").session, None);

        let (scoped, _) = scope_to(nodes, Vec::new(), Some(&sessions::short_id(mine)), None);
        let ids: Vec<&str> = scoped.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&mine) && ids.contains(&"0aaaaaaa-0000-7000-8000-000000000002"));
        assert!(!ids.contains(&"0bbbbbbb-0000-7000-8000-0000000000b2"));
        assert!(!ids.contains(&"0ccccccc-0000-7000-8000-0000000000c2"));
        assert!(!ids.contains(&other));
    }

    #[test]
    fn supervisor_advice_is_an_supervisor_event_addressed_to_its_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_for(dir.path());
        std::fs::create_dir_all(state.root()).expect("mkdir");
        let edge = |from: &str| super::super::mail::MailEdge {
            ts: 40,
            from_session: from.into(),
            to_session: Some("ce5aff8d-7289-4c63-aaa1-69f7ab9e421e".into()),
            to_label: "ce5aff8d".into(),
            unread: true,
            first_line: "Advisory from the on-call supervisor".into(),
            topic: Some("supervisor".into()),
        };
        let forged = super::super::mail::MailEdge {
            topic: None,
            ..edge("supervisor")
        };
        let plain = merged_events_with_mail(&state, &[forged]);
        assert_eq!(plain[0].kind, "mail");
        let events = merged_events_with_mail(&state, &[edge("supervisor"), edge("peer")]);
        let kinds: Vec<(&str, &str)> = events
            .iter()
            .map(|e| (e.actor.as_str(), e.kind.as_str()))
            .collect();
        assert_eq!(kinds, vec![("supervisor", "supervisor"), ("peer", "mail")]);
        assert_eq!(events[0].to.as_deref(), Some("ce5aff8d"));
        assert!(events[0].summary.starts_with("\u{2192} ce5aff8d"));
    }
}
