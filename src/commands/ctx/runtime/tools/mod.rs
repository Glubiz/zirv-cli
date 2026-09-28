//! Native coding and knowledge tool service (issues #474-#475, roadmap N05-N06).
//!
//! Provider output supplies only a stable tool name and JSON arguments. This
//! module validates that payload against a closed typed registry, converts it
//! to an N04 [`ExecutionAction`], obtains effect-time authorization, and only
//! then reaches filesystem/process code. Large results stream into the
//! existing output store and return opaque retrieval ids. Every invocation
//! returns a bounded receipt with an explicit retry/reconciliation contract.

mod capability;
pub mod delegation;
mod files;
mod memory;
mod process;
mod registry;
mod skill;
pub mod team;
mod workflow;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use self::process::{ProcessLimits, ProcessManager};
use self::registry::ParsedTool;
pub(crate) use self::registry::{ToolDefinition, ToolRegistry};
use super::capabilities::{CapabilityError, CapabilityServices};
use super::enforcement::{
    ApprovalGrant, ApprovalRequest, Authorization, BrokerError, ExecutionAction, ExecutionBroker,
    ProcessEffects, ProcessInvocation,
};
use super::journal::{
    ContentRef, EventScope, ExecutionId, ExecutionState, Journal, JournalSessionId, ToolCallId,
};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::output::{self, CompactionScope, StreamingCapture};
use crate::commands::ctx::state::{self, StateDir, now_ms};

pub const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_PROCESSES: usize = 16;

fn resolve_frontend_runner(
    program: &OsStr,
    cwd: &Path,
) -> crate::commands::ctx::CtxResult<PathBuf> {
    let program_path = Path::new(program);
    let has_directory = program_path.components().count() > 1;
    let candidates = if has_directory {
        vec![if program_path.is_absolute() {
            program_path.to_path_buf()
        } else {
            cwd.join(program_path)
        }]
    } else {
        let parent_cwd = std::env::current_dir()?;
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| {
                let directory = if directory.is_absolute() {
                    directory
                } else {
                    parent_cwd.join(directory)
                };
                directory.join(program_path)
            })
            .collect()
    };

    // Canonical paths only: a symlinked runner (Homebrew, nvm) must be launched
    // and admitted by the target it resolves to, or isolation cannot exec it.
    for candidate in candidates {
        if frontend_runner_is_executable(&candidate) {
            return Ok(std::fs::canonicalize(&candidate)?);
        }
        #[cfg(windows)]
        for extension in std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|extension| !extension.is_empty())
        {
            let extended = PathBuf::from(format!("{}{extension}", candidate.display()));
            if frontend_runner_is_executable(&extended) {
                return Ok(std::fs::canonicalize(&extended)?);
            }
        }
    }
    Err(format!(
        "frontend runner '{}' is unavailable",
        program.to_string_lossy()
    )
    .into())
}

fn frontend_runner_is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn fixed_frontend_path(runner: &Path) -> crate::commands::ctx::CtxResult<String> {
    let mut directories = vec![
        runner
            .parent()
            .ok_or("frontend runner has no parent directory")?
            .to_path_buf(),
    ];
    #[cfg(unix)]
    directories.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    #[cfg(windows)]
    {
        let windows = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        directories.extend([
            windows.join("System32"),
            windows.clone(),
            windows.join("System32").join("Wbem"),
        ]);
    }
    let mut unique = Vec::new();
    for directory in directories {
        if !unique.contains(&directory) {
            unique.push(directory);
        }
    }
    std::env::join_paths(unique)
        .map_err(|error| error.to_string())?
        .into_string()
        .map_err(|_| "frontend runner path is not valid Unicode".into())
}

pub const FILE_READ: &str = "file_read";
pub const DIRECTORY_LIST: &str = "directory_list";
pub const GLOB_SEARCH: &str = "glob_search";
pub const TEXT_SEARCH: &str = "text_search";
pub const FILE_WRITE: &str = "file_write";
pub const APPLY_PATCH: &str = "apply_patch";
pub const PROCESS_START: &str = "process_start";
pub const PROCESS_POLL: &str = "process_poll";
pub const PROCESS_WAIT: &str = "process_wait";
pub const PROCESS_WRITE: &str = "process_write";
pub const PROCESS_TERMINATE: &str = "process_terminate";
pub const OUTPUT_READ: &str = "output_read";
pub const MEMORY_RECALL: &str = "memory_recall";
pub const MEMORY_REMEMBER: &str = "memory_remember";
pub const MEMORY_FORGET: &str = "memory_forget";
pub const CONTEXT_SEARCH: &str = "context_search";
pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
pub const BROWSER_CAPTURE: &str = "browser_capture";
pub const BROWSER_INSPECT: &str = "browser_inspect";
pub const DIAGNOSTICS_REPORT: &str = "diagnostics_report";
pub const CAPABILITY_REPORT: &str = "capability_report";
pub const ARTIFACT_REGISTER: &str = "artifact_register";
pub const ARTIFACT_PRESENT: &str = "artifact_present";
pub const FRONTEND_RENDER: &str = "frontend_render";
pub const FRONTEND_REVIEW: &str = "frontend_review";
pub const MCP_LIST: &str = "mcp_list";
pub const MCP_DESCRIBE: &str = "mcp_describe";
pub const MCP_CALL: &str = "mcp_call";
pub const WORKFLOW_STATUS: &str = "workflow_status";
pub const WORKFLOW_CONTEXT: &str = "workflow_context";
pub const WORKFLOW_ADVANCE: &str = "workflow_advance";
pub const WORKFLOW_APPROVE: &str = "workflow_approve";
pub const WORKFLOW_LIST: &str = "workflow_list";
pub const WORKFLOW_START: &str = "workflow_start";
/// Issue #539 chunk E1: an agent's own skill discovery/load/resource tools,
/// mirrored on the read-only MCP bridge (`ctx::mcp`) with the same names,
/// arg shapes and result shapes -- both surfaces call
/// `workflow::skill_tools` rather than rendering a skill twice.
pub const SKILL_LIST: &str = "skill_list";
pub const SKILL_LOAD: &str = "skill_load";
pub const SKILL_READ_RESOURCE: &str = "skill_read_resource";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    Immediate,
    BackgroundProcess,
    ProcessControl,
    Retrieval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceClaimKind {
    ReadRoot,
    WorktreeWrite,
    OutsideWrite,
    GitMetadata,
    Network,
    OutputStore,
    MemoryStore,
    SearchIndex,
    /// Issue #479: the shared delegation store -- task cards, worktree write
    /// claims, provider reservations and the durable delegation records
    /// themselves. Named separately from `WorktreeWrite` because owning a
    /// delegation is not the same claim as holding a checkout.
    DelegationStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationContract {
    BeforeEffect,
    AtomicCommit,
    ProcessTree,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPolicy {
    Safe,
    Reconcile,
    NeverAfterStart,
}

/// Shared by every `tools` submodule; a trim-before-check caller (team.rs,
/// delegation.rs) passes `value.trim()` in to get that behaviour.
pub(super) fn non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

pub(super) fn valid_idempotency(value: &str) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > 256 || value.contains('\0') {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "idempotency_key must contain 1..=256 non-NUL bytes",
        ))
    } else {
        Ok(())
    }
}

/// [`valid_idempotency`] for callers (files.rs, process.rs) that treat a
/// whitespace-only key as empty; length and the NUL check still apply to the
/// untrimmed key, matching those modules' own former checks exactly.
pub(super) fn valid_idempotency_trimmed(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() || value.len() > 256 || value.contains('\0') {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "idempotency_key must contain 1..=256 non-NUL bytes",
        ))
    } else {
        Ok(())
    }
}

/// Maps MCP failures onto the tool vocabulary without turning an uncertain
/// external effect into a clean failure a caller could retry.
fn mcp_error(error: super::mcp::McpError) -> ToolError {
    use super::mcp::McpError;

    match error {
        McpError::Cancelled | McpError::OutcomeUnknown(_) => ToolError {
            code: ToolErrorCode::Internal,
            message: error.to_string(),
            approval: None,
            outcome_unknown: true,
        },
        McpError::StaleTool(_) => {
            ToolError::new(ToolErrorCode::PreconditionFailed, error.to_string())
        }
        McpError::Unavailable(_) | McpError::AuthenticationRejected(_) => {
            ToolError::new(ToolErrorCode::PreconditionFailed, error.to_string())
        }
        McpError::Timeout(_) => ToolError::new(ToolErrorCode::ResourceBusy, error.to_string()),
        McpError::Server { .. } | McpError::Protocol(_) => {
            ToolError::new(ToolErrorCode::UnsupportedContent, error.to_string())
        }
        McpError::Transport(_) => ToolError::new(ToolErrorCode::Io, error.to_string()),
    }
}

impl From<CapabilityError> for ToolError {
    fn from(error: CapabilityError) -> Self {
        let code = match &error {
            CapabilityError::Unavailable(_) => ToolErrorCode::PreconditionFailed,
            CapabilityError::Denied(_) => ToolErrorCode::AuthorizationDenied,
            CapabilityError::Backend(_) => ToolErrorCode::Io,
        };
        Self::new(code, error.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReceiptState {
    Completed,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolReceipt {
    pub receipt_id: String,
    pub tool: String,
    pub state: ToolReceiptState,
    pub retry: RetryPolicy,
    pub result: Option<Value>,
    pub error: Option<ToolError>,
    pub policy_fingerprint: Option<String>,
    pub approved_by: Option<String>,
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    UnknownTool,
    InvalidArguments,
    AuthorizationDenied,
    ApprovalRequired,
    IsolationUnavailable,
    PreconditionFailed,
    UnsupportedContent,
    ResourceBusy,
    UnknownProcess,
    ProcessClosed,
    OutputLimit,
    Io,
    Journal,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolError {
    pub code: ToolErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<Box<ApprovalRequest>>,
    pub outcome_unknown: bool,
}

impl ToolError {
    pub(super) fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            approval: None,
            outcome_unknown: false,
        }
    }

    pub(super) fn io(error: std::io::Error) -> Self {
        Self::new(ToolErrorCode::Io, error.to_string())
    }

    pub(super) fn external(error: impl Display) -> Self {
        Self::new(ToolErrorCode::Internal, error.to_string())
    }

    fn unknown_outcome(message: impl Into<String>) -> Self {
        Self {
            code: ToolErrorCode::Journal,
            message: message.into(),
            approval: None,
            outcome_unknown: true,
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<BrokerError> for ToolError {
    fn from(error: BrokerError) -> Self {
        let code = match &error {
            BrokerError::ApprovalRequired(_) | BrokerError::ApprovalUnavailable(_) => {
                ToolErrorCode::ApprovalRequired
            }
            BrokerError::IsolationUnavailable(_) => ToolErrorCode::IsolationUnavailable,
            BrokerError::WriterPermit(_) => ToolErrorCode::ResourceBusy,
            BrokerError::Denied(_)
            | BrokerError::ProtectedPath(_)
            | BrokerError::Scope(_)
            | BrokerError::Identity(_)
            | BrokerError::StaleGeneration { .. }
            | BrokerError::InvalidApproval(_) => ToolErrorCode::AuthorizationDenied,
            BrokerError::InvalidAction(_) => ToolErrorCode::InvalidArguments,
            BrokerError::PolicyUnavailable(_) | BrokerError::Internal(_) => ToolErrorCode::Internal,
        };
        let approval = match &error {
            BrokerError::ApprovalRequired(request)
            | BrokerError::ApprovalUnavailable(request)
            | BrokerError::InvalidApproval(request) => Some(request.clone()),
            _ => None,
        };
        Self {
            code,
            message: error.to_string(),
            approval,
            outcome_unknown: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolLimits {
    pub max_inline_bytes: usize,
    pub max_output_read_bytes: usize,
    pub max_processes: usize,
    process: ProcessLimits,
}

impl ToolLimits {
    pub fn from_config(config: &CtxConfig) -> Self {
        let max_inline_bytes = config.search.max_output_bytes;
        Self {
            max_inline_bytes,
            max_output_read_bytes: config.search.max_output_bytes,
            max_processes: DEFAULT_MAX_PROCESSES,
            process: ProcessLimits {
                max_inline_bytes,
                max_processes: DEFAULT_MAX_PROCESSES,
                max_summary_bytes: config.output.max_summary_bytes,
                max_heavy_operations: config.supervise.max_heavy_operations,
                heavy_patterns: config.supervise.heavy_command_patterns.clone(),
                output_filter: config.output.filter.clone(),
                extra_verbatim: config.output.verbatim.clone(),
                compact_search: config.output.compact_search,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn testing() -> Self {
        Self {
            max_inline_bytes: 1024,
            max_output_read_bytes: 2048,
            max_processes: 4,
            process: ProcessLimits {
                max_inline_bytes: 1024,
                max_processes: 4,
                max_summary_bytes: 4096,
                max_heavy_operations: 1,
                heavy_patterns: Vec::new(),
                output_filter: Vec::new(),
                extra_verbatim: Vec::new(),
                compact_search: true,
            },
        }
    }
}

pub struct JournalExecution<'a> {
    pub journal: &'a mut Journal,
    pub session: JournalSessionId,
    pub generation: u64,
    pub scope: EventScope,
    pub tool_call: ToolCallId,
    pub execution: ExecutionId,
}

pub struct NativeToolClient {
    registry: ToolRegistry,
    broker: ExecutionBroker,
    state: StateDir,
    repo: PathBuf,
    limits: ToolLimits,
    processes: ProcessManager,
    /// Issue #479: how the `delegate` tool actually starts a worker. The
    /// production value is `delegation::AgentLauncher`, i.e. one
    /// `agent::run_with` call -- the exact function `zirv agent` runs -- so
    /// the tool and the CLI verb are one code path with one set of gates. A
    /// test substitutes a launcher that starts nothing.
    launcher: Box<dyn crate::commands::ctx::delegation::WorkerLauncher>,
    launch_env: BTreeMap<String, String>,
    services: CapabilityServices,
}

impl std::fmt::Debug for NativeToolClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeToolClient")
            .field("registry", &self.registry)
            .field("broker", &self.broker)
            .field("repo", &self.repo)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl NativeToolClient {
    pub fn new(
        broker: ExecutionBroker,
        state: StateDir,
        repo: PathBuf,
        limits: ToolLimits,
    ) -> Self {
        let processes = ProcessManager::new(state.clone(), repo.clone(), limits.process.clone());
        let launcher =
            Box::new(crate::commands::ctx::delegation::AgentLauncher { repo: repo.clone() });
        Self {
            registry: ToolRegistry::native(),
            broker,
            state,
            repo,
            limits,
            processes,
            launcher,
            launch_env: BTreeMap::new(),
            services: CapabilityServices::default(),
        }
    }

    pub(crate) fn install_launch_env(&mut self, env: crate::commands::ctx::config::EnvLookup<'_>) {
        self.launch_env.clear();
        for key in [
            crate::commands::ctx::agent::ENVELOPE_ENV,
            crate::commands::ctx::agent::PRINCIPAL_ENV,
        ] {
            if let Some(value) = env(key) {
                self.launch_env.insert(key.to_string(), value);
            }
        }
    }

    pub(crate) fn lock_generation(
        &self,
    ) -> Result<Box<dyn super::enforcement::GenerationLease>, super::enforcement::BrokerError> {
        self.broker.lock_generation()
    }

    /// Replaces the worker launcher the `delegate` tool uses. Exists so a
    /// deterministic test can drive the whole delegation tool surface without
    /// starting a real worker; production always keeps the default.
    #[cfg(test)]
    pub fn with_launcher(
        mut self,
        launcher: Box<dyn crate::commands::ctx::delegation::WorkerLauncher>,
    ) -> Self {
        self.launcher = launcher;
        self
    }

    /// Attaches the operator's configured MCP/web/browser backends and
    /// promotes a SMALL MCP catalogue into the registry. Above
    /// `max_inline_mcp_tools`, discovered tools stay reachable only through
    /// `mcp_list`/`mcp_describe`/`mcp_call`, which is what keeps a large
    /// toolset out of every model request.
    ///
    /// Connecting is best-effort by construction: a server that cannot be
    /// reached leaves its tools unregistered and its integration row
    /// unverified, and never prevents the session from starting.
    pub fn with_capabilities(mut self, mut services: CapabilityServices) -> Self {
        let names = services.server_names();
        let budget = services.max_inline_mcp_tools();
        for name in names {
            let Ok(client) = services.client(&name, &self.broker) else {
                continue;
            };
            if client.catalogue().len() > budget {
                continue;
            }
            let effects = client.effects().clone();
            let entries: Vec<super::mcp::McpToolEntry> =
                client.catalogue().entries().cloned().collect();
            self.registry.clear_mcp(&name);
            for entry in entries {
                let _ = self.registry.register_mcp(&name, &entry, effects.clone());
            }
        }
        self.services = services;
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn services(&self) -> &CapabilityServices {
        &self.services
    }

    pub fn execute(
        &mut self,
        name: &str,
        arguments: Value,
        grant: Option<&ApprovalGrant>,
        journal: Option<JournalExecution<'_>>,
    ) -> ToolReceipt {
        let generation = match self.broker.lock_generation() {
            Ok(generation) => generation,
            Err(error) => {
                return failed_receipt(name, RetryPolicy::NeverAfterStart, error.into(), now_ms());
            }
        };
        let receipt = self.execute_unfenced(name, arguments, grant, journal);
        drop(generation);
        receipt
    }

    pub(crate) fn execute_unfenced(
        &mut self,
        name: &str,
        arguments: Value,
        grant: Option<&ApprovalGrant>,
        mut journal: Option<JournalExecution<'_>>,
    ) -> ToolReceipt {
        let started_at_ms = now_ms();
        let parsed = match self.registry.parse(name, arguments) {
            Ok(parsed) => parsed,
            Err(error) => return failed_receipt(name, RetryPolicy::Safe, error, started_at_ms),
        };
        let retry = parsed.retry_policy();
        let action = match parsed.action() {
            Ok(action)
                if matches!(&parsed, ParsedTool::McpList(_) | ParsedTool::McpDescribe(_)) =>
            {
                action
            }
            Ok(action) => self.with_declared_effects(action),
            Err(error) => return failed_receipt(name, retry, error, started_at_ms),
        };
        if matches!(
            &parsed,
            ParsedTool::BrowserCapture(_) | ParsedTool::BrowserInspect(_)
        ) && let Err(error) = self.broker.authorize_browser_network(name)
        {
            return failed_receipt(name, retry, error.into(), started_at_ms);
        }
        let authorization = match self.broker.authorize(&action, grant) {
            Ok(authorization) => authorization,
            Err(error) => return failed_receipt(name, retry, error.into(), started_at_ms),
        };

        if let Some(record) = journal.as_mut()
            && let Err(error) = journal_start(record)
        {
            return failed_receipt(
                name,
                retry,
                ToolError::new(ToolErrorCode::Journal, error.to_string()),
                started_at_ms,
            );
        }

        let result = self.dispatch(parsed, &action, &authorization);
        let completed_at_ms = now_ms();
        let mut receipt = match result {
            Ok(result) => ToolReceipt {
                receipt_id: uuid::Uuid::new_v4().simple().to_string(),
                tool: name.to_string(),
                state: ToolReceiptState::Completed,
                retry,
                result: Some(result),
                error: None,
                policy_fingerprint: Some(authorization.policy_fingerprint().to_string()),
                approved_by: authorization.approved_by().map(str::to_string),
                started_at_ms,
                completed_at_ms,
            },
            Err(error) => ToolReceipt {
                receipt_id: uuid::Uuid::new_v4().simple().to_string(),
                tool: name.to_string(),
                state: if error.outcome_unknown {
                    ToolReceiptState::OutcomeUnknown
                } else {
                    ToolReceiptState::Failed
                },
                retry,
                result: None,
                error: Some(error),
                policy_fingerprint: Some(authorization.policy_fingerprint().to_string()),
                approved_by: authorization.approved_by().map(str::to_string),
                started_at_ms,
                completed_at_ms,
            },
        };
        if let Some(record) = journal.as_mut()
            && let Err(error) = journal_finish(record, &receipt)
        {
            receipt.state = ToolReceiptState::OutcomeUnknown;
            receipt.retry = RetryPolicy::NeverAfterStart;
            receipt.result = None;
            receipt.error = Some(ToolError::unknown_outcome(format!(
                "tool effect finished but its durable receipt failed: {error}"
            )));
        }
        receipt
    }

    fn dispatch(
        &mut self,
        parsed: ParsedTool,
        action: &ExecutionAction,
        authorization: &Authorization,
    ) -> Result<Value, ToolError> {
        match parsed {
            ParsedTool::ReadFile(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::read_file(path, &args, self.limits.max_inline_bytes)?)
            }
            ParsedTool::DirectoryList(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::list_directory(path, &args)?)
            }
            ParsedTool::Glob(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::glob(path, &args)?)
            }
            ParsedTool::Search(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::search(path, &args, authorization.protected_paths())?)
            }
            ParsedTool::WriteFile(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::write_file(path, &args)?)
            }
            ParsedTool::ApplyPatch(args) => {
                let path = authorized_path(authorization)?;
                self.finish_file(files::apply_patch(path, &args)?)
            }
            ParsedTool::ProcessStart(args) => {
                let launch = self
                    .broker
                    .prepare_process(action, authorization)
                    .map_err(ToolError::from)?;
                let snapshot = self.processes.start(launch, &args)?;
                ProcessManager::output_json(snapshot)
            }
            ParsedTool::ProcessPoll(args) => {
                ProcessManager::output_json(self.processes.poll(&args.handle)?)
            }
            ParsedTool::ProcessWait(args) => {
                ProcessManager::output_json(self.processes.wait(&args.handle, args.wait_ms)?)
            }
            ParsedTool::ProcessWrite(args) => ProcessManager::output_json(
                self.processes
                    .write_input(&args.handle, &args.input, args.close)?,
            ),
            ParsedTool::ProcessTerminate(args) => {
                ProcessManager::output_json(self.processes.terminate(&args.handle)?)
            }
            ParsedTool::OutputRead(args) => {
                let text = output::show_captured(
                    &self.state,
                    &self.repo,
                    args.id,
                    args.range,
                    args.bytes,
                    self.limits.max_output_read_bytes,
                )
                .map_err(ToolError::external)?;
                Ok(json!({ "content": text }))
            }
            ParsedTool::MemoryRecall(args) => self.recall_memory(args),
            ParsedTool::MemoryRemember(args) => self.remember_memory(args),
            ParsedTool::MemoryForget(args) => self.forget_memory(args),
            ParsedTool::ContextSearch(args) => self.search_context(args),
            ParsedTool::Delegate(args) => self.delegate(args),
            ParsedTool::Send(args) => self.send_to_worker(args),
            ParsedTool::Wait(args) => self.wait_for_worker(args),
            ParsedTool::Result(args) => self.worker_result(args),
            ParsedTool::FollowUp(args) => self.follow_up(args),
            ParsedTool::Interrupt(args) => self.interrupt_worker(args),
            ParsedTool::Close(args) => self.close_worker(args),
            ParsedTool::WebSearch(args) => self.bounded(
                self.web()?.search(&args.query).map_err(ToolError::from)?,
                "body",
                &["web_search", &args.query],
            ),
            ParsedTool::WebFetch(args) => self.bounded(
                self.web()?.fetch(&args.url).map_err(ToolError::from)?,
                "body",
                &["web_fetch", &args.url],
            ),
            ParsedTool::BrowserCapture(args) => {
                let output = capability::evidence_root(&self.state, &self.repo).join(format!(
                    "{}-{}x{}.png",
                    capability::evidence_slug(&args.label),
                    args.width,
                    args.height
                ));
                self.browser()?
                    .capture(&args.url, &output, args.width, args.height)
                    .map_err(ToolError::from)
            }
            ParsedTool::BrowserInspect(args) => self.bounded(
                self.browser()?
                    .inspect(&args.url)
                    .map_err(ToolError::from)?,
                "dom",
                &["browser_inspect", &args.url],
            ),
            ParsedTool::DiagnosticsReport(_) => {
                Ok(super::capabilities::diagnostics_report(&self.repo))
            }
            ParsedTool::CapabilityReport(_) => self.capability_report(),
            ParsedTool::ArtifactRegister(args) => {
                let path = authorized_path(authorization)?;
                let record = crate::commands::workflow::artifact::register(
                    &self.state,
                    &self.repo,
                    path,
                    args.kind.map(capability::ArtifactKindArg::kind),
                    args.workflow_id.clone(),
                )
                .map_err(ToolError::external)?;
                serde_json::to_value(record).map_err(ToolError::external)
            }
            ParsedTool::ArtifactPresent(args) => self.present_artifact(&args),
            ParsedTool::FrontendRender(_) => {
                let report = crate::commands::workflow::frontend_render::render_with_launcher(
                    &self.state,
                    &self.repo,
                    |command| self.launch_frontend_server(command),
                )
                .map_err(ToolError::external)?;
                serde_json::to_value(report).map_err(ToolError::external)
            }
            ParsedTool::FrontendReview(args) => {
                let review = crate::commands::workflow::frontend_render::review(
                    &self.state,
                    &self.repo,
                    &crate::commands::workflow::frontend_render::VisualReviewArgs {
                        repo: Some(self.repo.clone()),
                        agent: args.agent.clone(),
                        model: args.model.clone(),
                        // Issue #484: a native session's own frontend review
                        // stays on the native runtime; it has no vendor CLI to
                        // fall back to.
                        runtime: super::RuntimeKind::Native.as_str().to_string(),
                        json: true,
                    },
                )
                .map_err(ToolError::external)?;
                serde_json::to_value(review).map_err(ToolError::external)
            }
            ParsedTool::McpList(args) => self.list_mcp(args.server.as_deref()),
            ParsedTool::McpDescribe(args) => {
                let value = self
                    .services
                    .client(&args.server, &self.broker)
                    .map_err(ToolError::from)?
                    .catalogue_mut()
                    .describe(&args.tool)
                    .map_err(mcp_error)?;
                Ok(value)
            }
            ParsedTool::McpCall(args) => self.call_mcp(&args),
            ParsedTool::WorkflowStatus(args) => self.workflow_status(args.id.as_deref()),
            ParsedTool::WorkflowContext(args) => self.workflow_context(args.id.as_deref()),
            ParsedTool::WorkflowAdvance(args) => self.workflow_advance(&args),
            ParsedTool::WorkflowApprove(args) => self.workflow_approve(args.id.as_deref()),
            ParsedTool::WorkflowList(_) => self.workflow_list(),
            ParsedTool::WorkflowStart(args) => self.workflow_start(&args),
            ParsedTool::TaskCreate(args) => self.task_create(&args),
            ParsedTool::TaskClaim(args) => self.task_claim(&args),
            ParsedTool::TaskList(args) => self.task_list(&args),
            ParsedTool::GroupCreate(args) => self.group_create(&args),
            ParsedTool::GroupStatus(args) => self.group_status(args.group.as_deref()),
            ParsedTool::ObjectiveStatus(_) => self.objective_status(),
            ParsedTool::TeamStatus(_) => self.team_status(),
            ParsedTool::TeamPlan(args) => self.team_plan(&args),
            ParsedTool::SkillList(args) => self.skill_list(&args),
            ParsedTool::SkillLoad(args) => self.skill_load(&args),
            ParsedTool::SkillReadResource(args) => self.skill_read_resource(&args),
        }
    }

    fn launch_frontend_server(
        &self,
        command: std::process::Command,
    ) -> crate::commands::ctx::CtxResult<std::process::Child> {
        let cwd = command
            .get_current_dir()
            .ok_or("frontend server command has no working directory")?;
        let runner = resolve_frontend_runner(command.get_program(), cwd)?;
        let mut environment: BTreeMap<String, String> = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        environment.retain(|key, _| !key.eq_ignore_ascii_case("PATH"));
        environment.insert("PATH".into(), fixed_frontend_path(&runner)?);
        let invocation = ProcessInvocation::Argv {
            program: runner.to_string_lossy().into_owned(),
            args: command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            cwd: cwd.to_path_buf(),
            environment,
        };
        let action = ExecutionAction::Process {
            invocation,
            effects: ProcessEffects {
                network: true,
                clean_environment: true,
                ..ProcessEffects::default()
            },
        };
        let authorization = self.broker.authorize(&action, None)?;
        let launch = self.broker.prepare_process(&action, &authorization)?;
        let mut child = std::process::Command::new(&launch.program);
        child
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .env_clear()
            .envs(&launch.environment)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        super::super::supervise::isolate_process_tree(&mut child);
        Ok(child.spawn()?)
    }

    fn ctx_config(&self) -> Result<CtxConfig, ToolError> {
        CtxConfig::load(&self.repo, &|key| std::env::var(key).ok()).map_err(ToolError::external)
    }

    /// The operator's native provider configuration, or `None` on a machine
    /// that has none. Absence is not an error here: a harness-runtime
    /// delegation needs no native route at all, and a native one fails with
    /// the configuration message `native_worker` already produces.
    fn native_config(&self) -> Option<crate::commands::ctx::provider::config::NativeConfig> {
        let home = crate::utils::home_dir().ok()?;
        crate::commands::ctx::provider::config::NativeConfig::load(&home, &self.repo).ok()?
    }

    /// Issue #541 chunk C follow-up: the registry `delegation::delegate`
    /// checks a requested manifest against, resolved HERE -- where the repo
    /// and the operator's home directory are already known -- rather than
    /// inside `delegate` itself, which must stay pure. Matches `zirv
    /// workflow team plan`'s own resolution (and the native `team_plan`
    /// tool's), so an operator-global or repository manifest a coordinator
    /// delegates with is admitted exactly like `team_plan`/the slash
    /// commands already admit it. `None` on a registry load failure --
    /// `delegate` falls back to its own built-in-only lookup rather than
    /// failing the whole delegation over an unrelated load error.
    fn manifest_registry(&self) -> Option<crate::commands::workflow::agents::AgentRegistry> {
        crate::commands::workflow::agents::AgentRegistry::load_for_repo(
            &self.repo,
            dirs::home_dir().as_deref(),
            true,
        )
        .ok()
    }
}

#[derive(Debug)]
pub(super) struct CapturePayload {
    bytes: Vec<u8>,
    command: Vec<String>,
    scope: CompactionScope,
}

impl CapturePayload {
    pub(super) fn new(bytes: Vec<u8>, command: Vec<String>, scope: CompactionScope) -> Self {
        Self {
            bytes,
            command,
            scope,
        }
    }
}

fn persist_capture(
    state: &StateDir,
    repo: &Path,
    capture: CapturePayload,
    max_summary_bytes: usize,
    filter: &[crate::commands::ctx::config::OutputFilterRule],
) -> Result<output::CapturedOutput, ToolError> {
    let mut stored = StreamingCapture::start(state, repo).map_err(ToolError::external)?;
    if let Err(error) = stored.append(&capture.bytes) {
        stored.abort();
        return Err(ToolError::external(error));
    }
    stored
        .finish(
            &capture.command,
            Some(0),
            max_summary_bytes,
            capture.scope,
            filter,
        )
        .map_err(ToolError::external)
}

fn authorized_path(authorization: &Authorization) -> Result<&Path, ToolError> {
    authorization
        .resolved_paths()
        .first()
        .map(PathBuf::as_path)
        .ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::Internal,
                "filesystem authorization did not resolve a target path",
            )
        })
}

fn journal_start(record: &mut JournalExecution<'_>) -> Result<(), super::journal::JournalError> {
    let committed_at = state::now_secs();
    let at_ms = Some(now_ms());
    record.journal.prepare_execution(
        &record.session,
        record.generation,
        &record.scope,
        record.execution.clone(),
        record.tool_call.clone(),
        at_ms,
        committed_at,
    )?;
    record.journal.transition_execution(
        &record.session,
        record.generation,
        &record.scope,
        &record.execution,
        ExecutionState::Started,
        None,
        None,
        at_ms,
        committed_at,
    )?;
    Ok(())
}

fn journal_finish(
    record: &mut JournalExecution<'_>,
    receipt: &ToolReceipt,
) -> Result<(), super::journal::JournalError> {
    let (state, result, detail) = match receipt.state {
        ToolReceiptState::Completed => (ExecutionState::Completed, Some(receipt), None),
        ToolReceiptState::Failed => (
            ExecutionState::Failed,
            Some(receipt),
            receipt.error.as_ref().map(|error| error.message.clone()),
        ),
        ToolReceiptState::OutcomeUnknown => (
            ExecutionState::OutcomeUnknown,
            None,
            receipt.error.as_ref().map(|error| error.message.clone()),
        ),
    };
    let result = result
        .map(serde_json::to_string)
        .transpose()?
        .map(|text| ContentRef::Inline { text });
    record.journal.transition_execution(
        &record.session,
        record.generation,
        &record.scope,
        &record.execution,
        state,
        result,
        detail,
        Some(now_ms()),
        state::now_secs(),
    )?;
    Ok(())
}

fn failed_receipt(
    name: &str,
    retry: RetryPolicy,
    error: ToolError,
    started_at_ms: u64,
) -> ToolReceipt {
    let state = if error.outcome_unknown {
        ToolReceiptState::OutcomeUnknown
    } else {
        ToolReceiptState::Failed
    };
    ToolReceipt {
        receipt_id: uuid::Uuid::new_v4().simple().to_string(),
        tool: name.to_string(),
        state,
        retry,
        result: None,
        error: Some(error),
        policy_fingerprint: None,
        approved_by: None,
        started_at_ms,
        completed_at_ms: now_ms(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    // Issue #550: repository-selected frontend servers must cross process isolation.
    fn frontend_dev_server_is_brokered_and_refuses_unavailable_isolation() {
        use std::os::unix::fs::PermissionsExt;

        use super::super::super::policy::{EffectivePolicy, Stance};
        use super::super::enforcement::{
            ApprovalAuthority, ApprovalMode, ExecutionIdentity, NetworkScope, PlatformIsolation,
            PolicySnapshot, ResourceClaims,
        };

        let root = tempfile::tempdir().expect("tempdir");
        let repo = std::fs::canonicalize(root.path()).expect("canonical repo");
        let home = repo.join("home");
        let bin = repo.join("bin");
        let state = StateDir::from_root(repo.join("state"));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::write(repo.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#)
            .expect("package");
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "package.json"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .expect("git fixture")
                    .success()
            );
        }
        let spawned = repo.join("dev-server-spawned");
        let npm = bin.join("npm");
        std::fs::write(
            &npm,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nprintf spawned > '{}'\nexit 1\n",
                spawned.display()
            ),
        )
        .expect("npm fixture");
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755))
            .expect("npm executable");
        let browser = bin.join("chromium");
        std::fs::write(&browser, "#!/bin/sh\nexit 0\n").expect("browser fixture");
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755))
            .expect("browser executable");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[("PATH", Some(&path))]);

        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        let broker = ExecutionBroker::new(
            ExecutionIdentity {
                session: "frontend-test".into(),
                short: "frontend".into(),
                generation: 1,
                role: "worker".into(),
                task: None,
            },
            ResourceClaims::new(&repo, &repo, state.root(), &home, NetworkScope::Any)
                .expect("claims"),
            ApprovalMode::Headless,
            std::sync::Arc::new(FixedPolicy(
                PolicySnapshot::new(policy, Default::default()).expect("policy"),
            )),
            std::sync::Arc::new(FixedFence),
            std::sync::Arc::new(ApprovalAuthority::new()),
            None,
            PlatformIsolation::Unavailable {
                platform: "test".into(),
                reason: "fixture unavailable".into(),
            },
            Default::default(),
        )
        .expect("broker");
        let mut client = NativeToolClient::new(broker, state, repo, ToolLimits::testing());

        let receipt = call(&mut client, FRONTEND_RENDER, json!({}));

        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
        assert!(
            !spawned.exists(),
            "dev server bypassed the broker: {receipt:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    // Issue #550: frontend children get no ambient env or unapproved network claim.
    fn frontend_child_has_clean_environment_and_requires_network_authority() {
        use std::os::unix::fs::PermissionsExt;

        use super::super::super::policy::{EffectivePolicy, Stance};
        use super::super::enforcement::{
            ApprovalAuthority, ApprovalMode, ExecutionIdentity, NetworkScope, PlatformIsolation,
            PolicySnapshot, ResourceClaims,
        };

        let root = tempfile::tempdir().expect("tempdir");
        let repo = std::fs::canonicalize(root.path()).expect("canonical repo");
        let home = repo.join("home");
        let bin = repo.join("bin");
        let state = StateDir::from_root(repo.join("state"));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&bin).expect("bin");
        // The runner is found through a symlinked PATH entry, as with Homebrew
        // or nvm; it must be launched and admitted by its canonical target.
        let bin_link = repo.join("bin-link");
        std::os::unix::fs::symlink(&bin, &bin_link).expect("bin symlink");
        let observed = repo.join("observed");
        let runner = bin.join("frontend-fixture");
        std::fs::write(
            &runner,
            format!(
                "#!/bin/sh\nresult=clean\nif [ \"${{FRONTEND_PARENT_VALUE+x}}\" = x ]; then result=leaked; fi\nprintf '%s\\n%s' \"$result\" \"$PATH\" > '{}'\n",
                observed.display()
            ),
        )
        .expect("runner fixture");
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
            .expect("runner executable");
        let sandbox = repo.join("sandbox");
        std::fs::write(
            &sandbox,
            format!(
                "#!/bin/sh\nrunner_root=missing\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --ro-bind) [ \"$2\" = '{}' ] && runner_root=present; shift 3 ;;\n    --setenv) export \"$2=$3\"; shift 3 ;;\n    --) shift; [ \"$runner_root\" = present ] || exit 90; exec \"$@\" ;;\n    *) shift ;;\n  esac\ndone\nexit 91\n",
                bin.display()
            ),
        )
        .expect("sandbox fixture");
        std::fs::set_permissions(&sandbox, std::fs::Permissions::from_mode(0o755))
            .expect("sandbox executable");
        let path = bin_link.to_string_lossy();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            ("PATH", Some(path.as_ref())),
            ("FRONTEND_PARENT_VALUE", Some("must-not-leak")),
        ]);
        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        let mut safety = super::super::super::safety::SafetyPolicy::default();
        safety.default = super::super::super::safety::Verdict::Allow;
        let make_client = |network| {
            let broker = ExecutionBroker::new(
                ExecutionIdentity {
                    session: "frontend-exec-test".into(),
                    short: "frontend".into(),
                    generation: 1,
                    role: "worker".into(),
                    task: None,
                },
                ResourceClaims::new(&repo, &repo, state.root(), &home, network).expect("claims"),
                ApprovalMode::Headless,
                std::sync::Arc::new(FixedPolicy(
                    PolicySnapshot::new(policy.clone(), safety.clone()).expect("policy"),
                )),
                std::sync::Arc::new(FixedFence),
                std::sync::Arc::new(ApprovalAuthority::new()),
                None,
                PlatformIsolation::LinuxBubblewrap {
                    executable: sandbox.clone(),
                },
                Default::default(),
            )
            .expect("broker");
            NativeToolClient::new(broker, state.clone(), repo.clone(), ToolLimits::testing())
        };
        let command = || {
            let mut command = std::process::Command::new("frontend-fixture");
            command.current_dir(&repo);
            command
        };

        let allowed = make_client(NetworkScope::Any);
        let mut child = allowed
            .launch_frontend_server(command())
            .expect("brokered frontend child");
        assert!(child.wait().expect("wait").success());
        assert_eq!(
            std::fs::read_to_string(&observed).expect("observation"),
            format!(
                "clean\n{}",
                fixed_frontend_path(&runner).expect("fixed path")
            )
        );

        std::fs::remove_file(&observed).expect("clear observation");
        let denied = make_client(NetworkScope::Denied);
        assert!(denied.launch_frontend_server(command()).is_err());
        assert!(
            !observed.exists(),
            "network-denied frontend child was spawned"
        );
    }

    #[test]
    fn limits_are_derived_from_operator_config() {
        let limits = ToolLimits::testing();
        assert_eq!(limits.max_processes, 4);
        assert_eq!(limits.process.max_inline_bytes, 1024);
    }

    use crate::commands::ctx::delegation as service;
    use crate::commands::ctx::runtime::enforcement::{
        ApprovalAuthority, ApprovalMode, ConfigPolicySource, ExecutionIdentity, NetworkScope,
        PlatformIsolation, ResourceClaims, StoredSeatFence,
    };
    use crate::commands::ctx::seat;

    pub(super) struct DelegationFixture {
        pub(super) _root: tempfile::TempDir,
        pub(super) state: StateDir,
        pub(super) repo: PathBuf,
        pub(super) client: NativeToolClient,
        pub(super) launches: std::sync::Arc<std::sync::Mutex<Vec<service::LaunchRequest>>>,
    }

    /// A writer lease for the fixture's own checkout. The production lease is
    /// `permit::HeavyPermit`, which takes a real per-tree claim; a test needs
    /// the same ANSWER ("this session may write this tree") without the
    /// machine-wide permit store, and the broker only ever asks `covers`.
    #[derive(Debug)]
    pub(super) struct FixtureWriter(pub(super) PathBuf);

    impl crate::commands::ctx::runtime::enforcement::WriterLease for FixtureWriter {
        fn covers(&self, worktree: &Path) -> bool {
            // Both sides are re-canonicalised: the broker normalises its own
            // worktree root on construction, and Windows has more than one
            // spelling of the same directory.
            let key = |path: &Path| {
                crate::commands::ctx::permit::tree_key(
                    &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
                )
            };
            key(&self.0) == key(worktree)
        }
    }

    /// A real `NativeToolClient` -- real registry, real broker, real seat
    /// fence -- with only the WORKER LAUNCH replaced, so the delegation tools
    /// are exercised through their production path without starting anything.
    pub(super) fn delegation_fixture(exit_code: i32) -> DelegationFixture {
        fixture_with(exit_code, "orchestrator", false)
    }

    /// The same fixture with the delegating seat's ROLE and its writer lease
    /// as parameters: issue #485's bounds are decided from exactly those two
    /// facts, so a test has to be able to vary them.
    pub(super) fn fixture_with(exit_code: i32, role: &str, writer: bool) -> DelegationFixture {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        let home = root.path().join("home");
        let state_root = root.path().join("state");
        for path in [&repo, &home, &state_root] {
            std::fs::create_dir_all(path).expect("dir");
        }
        let repo = std::fs::canonicalize(&repo).expect("canonical repo");
        let state = StateDir::from_root(state_root);

        // The seat record the native session's own generation fence reads.
        seat::store(
            &state,
            &seat::Seat {
                short: "nativ001".to_string(),
                session: "native-session-1".to_string(),
                generation: 1,
                agent: "native".to_string(),
                model: None,
                provider: "fixture".to_string(),
                role: role.to_string(),
                pinned: false,
                phase: Default::default(),
                visited: Vec::new(),
                last_rollover_at: None,
                rollover_failures: 0,
                failed_rollover_observed_at: None,
                pending: None,
                displaced: None,
                created_at: 1,
                updated_at: 1,
                runtime: super::super::RuntimeKind::Native,
            },
        )
        .expect("seat");

        let broker = ExecutionBroker::new(
            ExecutionIdentity {
                session: "native-session-1".to_string(),
                short: "nativ001".to_string(),
                generation: 1,
                role: role.to_string(),
                task: None,
            },
            ResourceClaims::new(&repo, &repo, state.root(), &home, NetworkScope::Denied)
                .expect("claims"),
            ApprovalMode::Headless,
            std::sync::Arc::new(ConfigPolicySource::new(repo.clone())),
            std::sync::Arc::new(StoredSeatFence::new(state.clone())),
            std::sync::Arc::new(ApprovalAuthority::new()),
            writer.then(|| {
                Box::new(FixtureWriter(repo.clone()))
                    as Box<dyn crate::commands::ctx::runtime::enforcement::WriterLease>
            }),
            PlatformIsolation::detect(),
            Default::default(),
        )
        .expect("broker");

        let launcher = service::RecordingLauncher {
            exit_code,
            ..Default::default()
        };
        let launches = launcher.launches.clone();
        let client =
            NativeToolClient::new(broker, state.clone(), repo.clone(), ToolLimits::testing())
                .with_launcher(Box::new(launcher));
        DelegationFixture {
            _root: root,
            state,
            repo,
            client,
            launches,
        }
    }

    pub(super) fn call(client: &mut NativeToolClient, name: &str, arguments: Value) -> ToolReceipt {
        client.execute(name, arguments, None, None)
    }

    pub(super) fn handle_from(receipt: ToolReceipt) -> String {
        receipt.result.expect("result")["delegation"]
            .as_str()
            .expect("delegation handle")
            .to_string()
    }

    pub(super) fn result_of(receipt: &ToolReceipt) -> &Value {
        receipt
            .result
            .as_ref()
            .unwrap_or_else(|| panic!("expected a result, got {:?}", receipt.error))
    }

    #[derive(Debug)]
    pub(super) struct FixedPolicy(pub(super) super::super::enforcement::PolicySnapshot);

    impl super::super::enforcement::PolicySource for FixedPolicy {
        fn current(
            &self,
        ) -> Result<super::super::enforcement::PolicySnapshot, super::super::enforcement::BrokerError>
        {
            Ok(self.0.clone())
        }
    }

    #[derive(Debug)]
    pub(super) struct FixedFence;

    impl super::super::enforcement::GenerationFence for FixedFence {
        fn verify(
            &self,
            _identity: &super::super::enforcement::ExecutionIdentity,
        ) -> Result<(), super::super::enforcement::BrokerError> {
            Ok(())
        }
    }
}
