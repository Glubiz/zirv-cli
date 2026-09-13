//! Native coding and knowledge tool service (issues #474-#475, roadmap N05-N06).
//!
//! Provider output supplies only a stable tool name and JSON arguments. This
//! module validates that payload against a closed typed registry, converts it
//! to an N04 [`ExecutionAction`], obtains effect-time authorization, and only
//! then reaches filesystem/process code. Large results stream into the
//! existing output store and return opaque retrieval ids. Every invocation
//! returns a bounded receipt with an explicit retry/reconciliation contract.

pub mod delegation;
mod files;
mod process;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::delegation::{
    CLOSE, DELEGATE, DelegateArgs, FOLLOW_UP, HandleArgs, INTERRUPT, MessageArgs, RESULT,
    ResultArgs, SEND, WAIT, WaitArgs,
};
use self::files::{
    ApplyPatchArgs, DirectoryArgs, FileOutcome, GlobArgs, ReadFileArgs, SearchArgs, WriteFileArgs,
};
use self::process::{
    ProcessHandleArgs, ProcessLimits, ProcessManager, ProcessStartArgs, ProcessWaitArgs,
    ProcessWriteArgs,
};
use super::enforcement::{
    ApprovalGrant, ApprovalRequest, Authorization, BrokerError, ExecutionAction, ExecutionBroker,
    ProcessInvocation,
};
use super::journal::{
    ContentRef, EventScope, ExecutionId, ExecutionState, Journal, JournalSessionId, ToolCallId,
};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::output::{self, CompactionScope, StreamingCapture};
use crate::commands::ctx::state::{self, StateDir};

pub const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_PROCESSES: usize = 16;

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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: Vec<String>,
    pub execution_mode: ToolExecutionMode,
    pub resource_claims: Vec<ResourceClaimKind>,
    pub cancellation: CancellationContract,
    pub retry: RetryPolicy,
    pub errors: Vec<ToolErrorCode>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolRegistry {
    definitions: BTreeMap<String, ToolDefinition>,
}

impl ToolRegistry {
    pub fn native() -> Self {
        let mut registry = Self::default();
        for definition in native_definitions() {
            let previous = registry
                .definitions
                .insert(definition.name.clone(), definition);
            debug_assert!(previous.is_none(), "native tool names must be unique");
        }
        registry
    }

    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.definitions.get(name)
    }

    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.definitions.values()
    }

    fn parse(&self, name: &str, arguments: Value) -> Result<ParsedTool, ToolError> {
        if self.get(name).is_none() {
            return Err(ToolError::new(
                ToolErrorCode::UnknownTool,
                format!("unknown native tool {name:?}"),
            ));
        }
        let bytes = serde_json::to_vec(&arguments)
            .map_err(ToolError::external)?
            .len();
        if bytes > MAX_TOOL_ARGUMENT_BYTES {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                format!("tool arguments are {bytes} bytes; limit is {MAX_TOOL_ARGUMENT_BYTES}"),
            ));
        }
        if !arguments.is_object() {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                "tool arguments must be one complete JSON object",
            ));
        }
        macro_rules! parse {
            ($kind:ident, $ty:ty) => {
                serde_json::from_value::<$ty>(arguments)
                    .map(ParsedTool::$kind)
                    .map_err(|error| {
                        ToolError::new(ToolErrorCode::InvalidArguments, error.to_string())
                    })
            };
        }
        let parsed = match name {
            FILE_READ => parse!(ReadFile, ReadFileArgs),
            DIRECTORY_LIST => parse!(DirectoryList, DirectoryArgs),
            GLOB_SEARCH => parse!(Glob, GlobArgs),
            TEXT_SEARCH => parse!(Search, SearchArgs),
            FILE_WRITE => parse!(WriteFile, WriteFileArgs),
            APPLY_PATCH => parse!(ApplyPatch, ApplyPatchArgs),
            PROCESS_START => parse!(ProcessStart, ProcessStartArgs),
            PROCESS_POLL => parse!(ProcessPoll, ProcessHandleArgs),
            PROCESS_WAIT => parse!(ProcessWait, ProcessWaitArgs),
            PROCESS_WRITE => parse!(ProcessWrite, ProcessWriteArgs),
            PROCESS_TERMINATE => parse!(ProcessTerminate, ProcessHandleArgs),
            OUTPUT_READ => parse!(OutputRead, OutputReadArgs),
            MEMORY_RECALL => parse!(MemoryRecall, MemoryRecallArgs),
            MEMORY_REMEMBER => parse!(MemoryRemember, MemoryRememberArgs),
            MEMORY_FORGET => parse!(MemoryForget, MemoryForgetArgs),
            CONTEXT_SEARCH => parse!(ContextSearch, ContextSearchArgs),
            DELEGATE => parse!(Delegate, DelegateArgs),
            SEND => parse!(Send, MessageArgs),
            WAIT => parse!(Wait, WaitArgs),
            RESULT => parse!(Result, ResultArgs),
            FOLLOW_UP => parse!(FollowUp, MessageArgs),
            INTERRUPT => parse!(Interrupt, HandleArgs),
            CLOSE => parse!(Close, HandleArgs),
            _ => unreachable!("registry membership and parser match stay in lockstep"),
        }?;
        parsed.validate()?;
        Ok(parsed)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputReadArgs {
    id: String,
    #[serde(default)]
    range: Option<String>,
    #[serde(default)]
    bytes: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemoryToolScope {
    Private,
    Global,
    Shared,
    #[default]
    Session,
}

impl MemoryToolScope {
    fn memory_scope(self) -> crate::commands::ctx::memory::MemoryScope {
        use crate::commands::ctx::memory::MemoryScope;
        match self {
            Self::Private => MemoryScope::Private,
            Self::Global => MemoryScope::Global,
            Self::Shared => MemoryScope::Shared,
            Self::Session => MemoryScope::Session,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Global => "global",
            Self::Shared => "shared",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryRecallArgs {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    scope: Option<MemoryToolScope>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryRememberArgs {
    key: String,
    text: String,
    #[serde(default)]
    scope: MemoryToolScope,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryForgetArgs {
    key: String,
    #[serde(default)]
    scope: MemoryToolScope,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextSearchArgs {
    query: String,
}

#[derive(Debug)]
enum ParsedTool {
    ReadFile(ReadFileArgs),
    DirectoryList(DirectoryArgs),
    Glob(GlobArgs),
    Search(SearchArgs),
    WriteFile(WriteFileArgs),
    ApplyPatch(ApplyPatchArgs),
    ProcessStart(ProcessStartArgs),
    ProcessPoll(ProcessHandleArgs),
    ProcessWait(ProcessWaitArgs),
    ProcessWrite(ProcessWriteArgs),
    ProcessTerminate(ProcessHandleArgs),
    OutputRead(OutputReadArgs),
    MemoryRecall(MemoryRecallArgs),
    MemoryRemember(MemoryRememberArgs),
    MemoryForget(MemoryForgetArgs),
    ContextSearch(ContextSearchArgs),
    Delegate(DelegateArgs),
    Send(MessageArgs),
    Wait(WaitArgs),
    Result(ResultArgs),
    FollowUp(MessageArgs),
    Interrupt(HandleArgs),
    Close(HandleArgs),
}

impl ParsedTool {
    fn validate(&self) -> Result<(), ToolError> {
        let non_empty_path = |path: &Path, field: &str| {
            if path.as_os_str().is_empty() {
                Err(ToolError::new(
                    ToolErrorCode::InvalidArguments,
                    format!("{field} must not be empty"),
                ))
            } else {
                Ok(())
            }
        };
        match self {
            Self::ReadFile(args) => non_empty_path(&args.path, "path"),
            Self::DirectoryList(args) => {
                non_empty_path(&args.path, "path")?;
                positive(args.max_results, "max_results")
            }
            Self::Glob(args) => {
                non_empty_path(&args.root, "root")?;
                non_empty(&args.pattern, "pattern")?;
                positive(args.max_results, "max_results")
            }
            Self::Search(args) => {
                non_empty_path(&args.root, "root")?;
                non_empty(&args.query, "query")?;
                positive(args.max_results, "max_results")
            }
            Self::WriteFile(args) => {
                non_empty_path(&args.path, "path")?;
                valid_idempotency(&args.idempotency_key)
            }
            Self::ApplyPatch(args) => {
                non_empty_path(&args.path, "path")?;
                non_empty(&args.expected_sha256, "expected_sha256")?;
                if args.operations.is_empty() {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "operations must not be empty",
                    ));
                }
                valid_idempotency(&args.idempotency_key)
            }
            Self::ProcessStart(args) => {
                non_empty(&args.program, "program")?;
                non_empty_path(&args.cwd, "cwd")?;
                if args
                    .shell_script
                    .as_ref()
                    .is_some_and(|script| script.is_empty())
                {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "shell_script must not be empty when supplied",
                    ));
                }
                if args.timeout_ms == Some(0) {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "timeout_ms must be positive",
                    ));
                }
                valid_idempotency(&args.idempotency_key)
            }
            Self::ProcessPoll(args) | Self::ProcessTerminate(args) => {
                non_empty(&args.handle, "handle")
            }
            Self::ProcessWait(args) => {
                non_empty(&args.handle, "handle")?;
                if args.wait_ms > 60_000 {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "wait_ms must not exceed 60000",
                    ));
                }
                Ok(())
            }
            Self::ProcessWrite(args) => non_empty(&args.handle, "handle"),
            Self::OutputRead(args) => non_empty(&args.id, "id"),
            Self::MemoryRecall(args) => {
                if args.key.as_deref().is_some_and(str::is_empty) {
                    return Err(ToolError::new(
                        ToolErrorCode::InvalidArguments,
                        "key must not be empty when supplied",
                    ));
                }
                Ok(())
            }
            Self::MemoryRemember(args) => {
                non_empty(&args.key, "key")?;
                non_empty(&args.text, "text")
            }
            Self::MemoryForget(args) => non_empty(&args.key, "key"),
            Self::ContextSearch(args) => non_empty(&args.query, "query"),
            Self::Delegate(args) => args.validate(),
            Self::Send(args) | Self::FollowUp(args) => {
                delegation::validate_handle(&args.delegation)?;
                non_empty(&args.message, "message")
            }
            Self::Wait(args) => delegation::validate_handle(&args.delegation),
            Self::Result(args) => delegation::validate_handle(&args.delegation),
            Self::Interrupt(args) | Self::Close(args) => {
                delegation::validate_handle(&args.delegation)
            }
        }
    }

    fn action(&self) -> ExecutionAction {
        match self {
            Self::ReadFile(args) => ExecutionAction::ReadFile {
                path: args.path.clone(),
            },
            Self::DirectoryList(args) => ExecutionAction::ReadFile {
                path: args.path.clone(),
            },
            Self::Glob(args) => ExecutionAction::ReadFile {
                path: args.root.clone(),
            },
            Self::Search(args) => ExecutionAction::ReadFile {
                path: args.root.clone(),
            },
            Self::WriteFile(args) => ExecutionAction::WriteFile {
                path: args.path.clone(),
            },
            Self::ApplyPatch(args) => ExecutionAction::WriteFile {
                path: args.path.clone(),
            },
            Self::ProcessStart(args) => {
                let invocation = match &args.shell_script {
                    Some(script) => ProcessInvocation::Shell {
                        program: args.program.clone(),
                        args: args.args.clone(),
                        script: script.clone(),
                        cwd: args.cwd.clone(),
                        environment: args.environment.clone(),
                    },
                    None => ProcessInvocation::Argv {
                        program: args.program.clone(),
                        args: args.args.clone(),
                        cwd: args.cwd.clone(),
                        environment: args.environment.clone(),
                    },
                };
                ExecutionAction::Process {
                    invocation,
                    effects: args.effects(),
                }
            }
            Self::ProcessPoll(args) => process_control(&args.handle, PROCESS_POLL),
            Self::ProcessWait(args) => process_control(&args.handle, PROCESS_WAIT),
            Self::ProcessWrite(args) => process_control(&args.handle, PROCESS_WRITE),
            Self::ProcessTerminate(args) => process_control(&args.handle, PROCESS_TERMINATE),
            Self::OutputRead(args) => ExecutionAction::OutputRead {
                id: args.id.clone(),
            },
            Self::MemoryRecall(args) => ExecutionAction::Knowledge {
                service: "memory".into(),
                operation: "recall".into(),
                scope: args.scope.map(|scope| scope.label().to_string()),
                key: args.key.clone(),
                write: false,
            },
            Self::MemoryRemember(args) => ExecutionAction::Knowledge {
                service: "memory".into(),
                operation: "remember".into(),
                scope: Some(args.scope.label().into()),
                key: Some(args.key.clone()),
                write: true,
            },
            Self::MemoryForget(args) => ExecutionAction::Knowledge {
                service: "memory".into(),
                operation: "forget".into(),
                scope: Some(args.scope.label().into()),
                key: Some(args.key.clone()),
                write: true,
            },
            Self::ContextSearch(args) => ExecutionAction::Knowledge {
                service: "context".into(),
                operation: "search".into(),
                scope: None,
                key: Some(args.query.clone()),
                write: false,
            },
            // Issue #479: every delegation tool crosses the broker's own
            // `Delegate` action, so a native session cannot delegate around
            // the seat fence and policy its other tools run behind. `role`
            // names the operation for the six that address an existing
            // delegation, and the requested worker role for `delegate`
            // itself; `task` is the shared card (or the handle being acted
            // on) the broker requires to be non-empty.
            Self::Delegate(args) => ExecutionAction::Delegate {
                role: args.role_or_default(),
                task: args.action_task(),
            },
            Self::Send(args) => ExecutionAction::Delegate {
                role: SEND.into(),
                task: args.delegation.clone(),
            },
            Self::Wait(args) => ExecutionAction::Delegate {
                role: WAIT.into(),
                task: args.delegation.clone(),
            },
            Self::Result(args) => ExecutionAction::Delegate {
                role: RESULT.into(),
                task: args.delegation.clone(),
            },
            Self::FollowUp(args) => ExecutionAction::Delegate {
                role: FOLLOW_UP.into(),
                task: args.delegation.clone(),
            },
            Self::Interrupt(args) => ExecutionAction::Delegate {
                role: INTERRUPT.into(),
                task: args.delegation.clone(),
            },
            Self::Close(args) => ExecutionAction::Delegate {
                role: CLOSE.into(),
                task: args.delegation.clone(),
            },
        }
    }

    fn retry_policy(&self) -> RetryPolicy {
        match self {
            Self::ReadFile(_)
            | Self::DirectoryList(_)
            | Self::Glob(_)
            | Self::Search(_)
            | Self::ProcessPoll(_)
            | Self::ProcessWait(_)
            | Self::OutputRead(_)
            | Self::MemoryRecall(_)
            | Self::ContextSearch(_)
            // Reading a delegation's own durable state changes nothing.
            | Self::Wait(_)
            | Self::Result(_) => RetryPolicy::Safe,
            Self::WriteFile(_)
            | Self::ApplyPatch(_)
            | Self::ProcessWrite(_)
            | Self::ProcessTerminate(_)
            | Self::MemoryRemember(_)
            | Self::MemoryForget(_)
            // A repeated send/follow-up/interrupt/close must be reconciled
            // against the durable record rather than blindly replayed.
            | Self::Send(_)
            | Self::FollowUp(_)
            | Self::Interrupt(_)
            | Self::Close(_) => RetryPolicy::Reconcile,
            // A worker that may already be running cannot be re-dispatched
            // on a retry: that is how one task gets paid for twice.
            Self::Delegate(_) => RetryPolicy::NeverAfterStart,
            Self::ProcessStart(args)
                if args.network || args.outside_write || args.git_push_or_destructive =>
            {
                RetryPolicy::NeverAfterStart
            }
            Self::ProcessStart(_) => RetryPolicy::Reconcile,
        }
    }
}

fn non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

fn positive(value: usize, field: &str) -> Result<(), ToolError> {
    if value == 0 {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must be positive"),
        ))
    } else {
        Ok(())
    }
}

fn valid_idempotency(value: &str) -> Result<(), ToolError> {
    if value.is_empty() || value.len() > 256 || value.contains('\0') {
        Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "idempotency_key must contain 1..=256 non-NUL bytes",
        ))
    } else {
        Ok(())
    }
}

fn process_control(handle: &str, operation: &str) -> ExecutionAction {
    ExecutionAction::ProcessControl {
        handle: handle.to_string(),
        operation: operation.to_string(),
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
    fn testing() -> Self {
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
        }
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

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn execute(
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
        let action = parsed.action();
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
                state: ToolReceiptState::Failed,
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
                self.finish_file(files::search(path, &args)?)
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
        }
    }

    // -- the delegation tools (issue #479, roadmap N10) -------------------
    //
    // Every one of these is a thin adaptor over the SAME `ctx::delegation`
    // service method the corresponding CLI verb calls. None of them contains
    // delegation logic of its own; that is the point.

    fn ctx_config(&self) -> Result<CtxConfig, ToolError> {
        CtxConfig::load(&self.repo, &|key| std::env::var(key).ok()).map_err(ToolError::external)
    }

    fn delegate(&mut self, args: DelegateArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let cfg = self.ctx_config()?;
        let request = service::LaunchRequest {
            runtime: args.runtime.kind(),
            target: args.target_or_default(),
            brief: args.brief.clone(),
            role: args.role_or_default(),
            task: args.task.clone(),
            group: args.group.clone(),
            workdir: args.workdir.as_ref().map(PathBuf::from),
            read_only: args.mode == delegation::ToolMode::ReadOnly,
            budget_tokens: args.budget_tokens,
            max_tool_calls: args.max_tool_calls,
        };
        let identity = self.broker.identity().clone();
        let (record, publication) = service::delegate(
            &self.state,
            &self.repo,
            &cfg,
            self.launcher.as_mut(),
            &request,
            Some(identity.session.clone()),
            &identity.short,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(json!({
            "delegation": record.handle.delegation,
            "attempt": record.handle.attempt,
            "runtime": record.handle.runtime.as_str(),
            "phase": record.phase.as_str(),
            "task": record.handle.task,
            "exit_code": record.exit_code,
            "delivery": publication.identity,
            "mailed": publication.mailed,
        }))
    }

    fn send_to_worker(&mut self, args: MessageArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let cfg = self.ctx_config()?;
        let dispatch = service::send(
            &self.state,
            &self.repo,
            &cfg,
            &args.delegation,
            &args.message,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(match dispatch {
            service::Dispatch::Delivered { .. } => json!({"delivered": true}),
            service::Dispatch::Queued { id, reason } => json!({
                "delivered": false,
                "queued": id,
                "reason": reason,
                "retry": "the message is durable and is delivered at the worker's next idle \
                          boundary; it is never typed into an open dialog",
            }),
        })
    }

    fn wait_for_worker(&mut self, args: WaitArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let now = state::now_secs();
        let deadline = now.saturating_add(args.bounded_secs());
        let outcome = service::wait(&self.state, &self.repo, &args.delegation, now, deadline)
            .map_err(ToolError::external)?;
        Ok(match outcome {
            service::WaitOutcome::Ready(record) => json!({
                "ready": true,
                "phase": record.phase.as_str(),
                "exit_code": record.exit_code,
            }),
            service::WaitOutcome::Pending => {
                json!({"ready": false, "deadline_secs": args.bounded_secs()})
            }
            service::WaitOutcome::TimedOut => json!({"ready": false, "timed_out": true}),
        })
    }

    fn worker_result(&mut self, args: ResultArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let manifest = service::result(
            &self.state,
            &self.repo,
            &args.delegation,
            args.bounded_bytes(),
        )
        .map_err(ToolError::external)?;
        serde_json::to_value(manifest).map_err(ToolError::external)
    }

    fn follow_up(&mut self, args: MessageArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let cfg = self.ctx_config()?;
        let continuation = service::follow_up(
            &self.state,
            &self.repo,
            &cfg,
            &args.delegation,
            &args.message,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(match continuation {
            service::Continuation::Directed { dispatch } => json!({
                "route": "directed",
                "delivered": matches!(dispatch, service::Dispatch::Delivered { .. }),
            }),
            service::Continuation::Resume {
                journal_session,
                attempt,
            } => json!({
                "route": "resume",
                "session": journal_session,
                "attempt": attempt,
            }),
            service::Continuation::Checkpoint { handoff } => json!({
                "route": "checkpoint",
                "handoff": handoff,
                "note": "no verified resume path; this is a replacement worker with none of the \
                         original's hidden context",
            }),
        })
    }

    fn interrupt_worker(&mut self, args: HandleArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let record =
            service::interrupt(&self.state, &self.repo, &args.delegation, state::now_secs())
                .map_err(ToolError::external)?;
        Ok(json!({
            "phase": record.phase.as_str(),
            "unknown_tool_outcomes": record.unknown_tool_outcomes,
        }))
    }

    fn close_worker(&mut self, args: HandleArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let record = service::close(&self.state, &self.repo, &args.delegation, state::now_secs())
            .map_err(ToolError::external)?;
        Ok(json!({
            "phase": record.phase.as_str(),
            "receipts": record.published,
            "unknown_tool_outcomes": record.unknown_tool_outcomes,
        }))
    }

    fn recall_memory(&self, args: MemoryRecallArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::memory::{self, MemoryScope};

        let cfg = CtxConfig::load(&self.repo, &|key| std::env::var(key).ok())
            .map_err(ToolError::external)?;
        let slug = state::repo_slug(&self.repo);
        let scopes: Vec<MemoryToolScope> = args.scope.map_or_else(
            || {
                vec![
                    MemoryToolScope::Session,
                    MemoryToolScope::Private,
                    MemoryToolScope::Global,
                    MemoryToolScope::Shared,
                ]
            },
            |scope| vec![scope],
        );
        let mut rows = Vec::new();
        for scope in scopes {
            let entries = match scope.memory_scope() {
                MemoryScope::Session if cfg.memory.session_enabled => {
                    memory::list_session(&self.state, &slug, &self.broker.identity().session)
                }
                MemoryScope::Session => Ok(Vec::new()),
                scope => memory::list_scoped(scope, &self.repo, &self.state, &slug, &cfg),
            }
            .map_err(ToolError::external)?;
            for (_, entry) in entries {
                if args.key.as_deref().is_none_or(|key| key == entry.key) {
                    rows.push(json!({"scope": scope.label(), "entry": entry}));
                }
            }
        }
        Ok(json!({"entries": rows}))
    }

    fn remember_memory(&self, args: MemoryRememberArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::memory::{self, Entry, MemoryScope};

        let cfg = CtxConfig::load(&self.repo, &|key| std::env::var(key).ok())
            .map_err(ToolError::external)?;
        let scope = args.scope.memory_scope();
        if !scope.enabled(&cfg) {
            return Err(ToolError::new(
                ToolErrorCode::AuthorizationDenied,
                format!("memory write disabled by {}", scope.disabled_reason(&cfg)),
            ));
        }
        let slug = state::repo_slug(&self.repo);
        let now = state::now_secs();
        let entry = Entry {
            key: args.key,
            written_by: format!("native:{}", self.broker.identity().short),
            written: now,
            verified: now,
            source: "explicit".into(),
            body: args.text,
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        match scope {
            MemoryScope::Session => memory::remember_session(
                &self.state,
                &slug,
                &self.broker.identity().session,
                &entry,
                &cfg,
            ),
            scope => memory::upsert_scoped(scope, &self.repo, &self.state, &slug, &cfg, &entry),
        }
        .map_err(ToolError::external)?;
        Ok(json!({
            "stored": true,
            "scope": args.scope.label(),
            "key": entry.key,
        }))
    }

    fn forget_memory(&self, args: MemoryForgetArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::memory::{self, MemoryScope};

        let slug = state::repo_slug(&self.repo);
        let removed = match args.scope.memory_scope() {
            MemoryScope::Session => memory::forget_session(
                &self.state,
                &slug,
                &self.broker.identity().session,
                &args.key,
            )
            .map_err(ToolError::external)?,
            scope => {
                memory::forget_scoped(scope, &self.repo, &self.state, &slug, &args.key)
                    .map_err(ToolError::external)?
                    .removed
            }
        };
        Ok(json!({
            "removed": removed,
            "scope": args.scope.label(),
            "key": args.key,
        }))
    }

    fn search_context(&self, args: ContextSearchArgs) -> Result<Value, ToolError> {
        let state_root = self.state.root().to_string_lossy().into_owned();
        let env = |key: &str| {
            if key == state::STATE_ENV {
                Some(state_root.clone())
            } else {
                std::env::var(key).ok()
            }
        };
        let mut output = Vec::new();
        let code = crate::commands::ctx::search::run_with(
            &crate::commands::ctx::search::SearchArgs {
                query: Some(args.query),
                around: None,
                session: None,
                all_repos: false,
                json: true,
            },
            &mut output,
            &self.repo,
            &env,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        if code != 0 {
            return Err(ToolError::new(
                ToolErrorCode::Internal,
                format!("context search exited with {code}"),
            ));
        }
        serde_json::from_slice(output.trim_ascii())
            .map_err(|error| ToolError::new(ToolErrorCode::Internal, error.to_string()))
    }

    fn finish_file(&self, mut outcome: FileOutcome) -> Result<Value, ToolError> {
        if let Some(capture) = outcome.capture.take() {
            let stored = persist_capture(
                &self.state,
                &self.repo,
                capture,
                self.limits.process.max_summary_bytes,
                &self.limits.process.output_filter,
            )?;
            let object = outcome.data.as_object_mut().ok_or_else(|| {
                ToolError::new(ToolErrorCode::Internal, "file result is not an object")
            })?;
            object.insert("output_id".into(), Value::String(stored.id));
            object.insert(
                "summary".into(),
                stored.summary.map(Value::String).unwrap_or(Value::Null),
            );
        }
        Ok(outcome.data)
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
    let (state, detail) = match receipt.state {
        ToolReceiptState::Completed => (ExecutionState::Completed, None),
        ToolReceiptState::Failed => (
            ExecutionState::Failed,
            receipt.error.as_ref().map(|error| error.message.clone()),
        ),
        ToolReceiptState::OutcomeUnknown => (ExecutionState::OutcomeUnknown, None),
    };
    let text = serde_json::to_string(receipt)?;
    record.journal.transition_execution(
        &record.session,
        record.generation,
        &record.scope,
        &record.execution,
        state,
        Some(ContentRef::Inline { text }),
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn definition(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: &[&str],
    execution_mode: ToolExecutionMode,
    resource_claims: &[ResourceClaimKind],
    lifecycle: (CancellationContract, RetryPolicy),
) -> ToolDefinition {
    let (cancellation, retry) = lifecycle;
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema,
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        execution_mode,
        resource_claims: resource_claims.to_vec(),
        cancellation,
        retry,
        errors: vec![
            ToolErrorCode::InvalidArguments,
            ToolErrorCode::AuthorizationDenied,
            ToolErrorCode::ApprovalRequired,
            ToolErrorCode::PreconditionFailed,
            ToolErrorCode::Io,
            ToolErrorCode::Internal,
        ],
    }
}

fn object_schema(required: &[&str], properties: Value) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    })
}

fn native_definitions() -> Vec<ToolDefinition> {
    let read_caps = ["tool_access"];
    let write_caps = ["tool_access", "repo_fs_write"];
    // Delegating spends another worker's budget and may hand it a checkout,
    // so it declares the write capability as well as tool access.
    let delegate_caps = ["tool_access", "delegation", "repo_fs_write"];
    let process_caps = [
        "tool_access",
        "shell_exec",
        "repo_fs_write",
        "outside_repo_fs_write",
        "network",
        "git_push_destructive",
    ];
    vec![
        definition(
            FILE_READ,
            "Read a bounded text range or inspect binary/image metadata.",
            object_schema(
                &["path"],
                json!({
                    "path": {"type":"string"},
                    "start_line": {"type":"integer","minimum":1},
                    "end_line": {"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            (CancellationContract::BeforeEffect, RetryPolicy::Safe),
        ),
        definition(
            DIRECTORY_LIST,
            "List a directory without following symlinked directories.",
            object_schema(
                &["path"],
                json!({
                    "path":{"type":"string"},
                    "recursive":{"type":"boolean"},
                    "max_results":{"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            (CancellationContract::BeforeEffect, RetryPolicy::Safe),
        ),
        definition(
            GLOB_SEARCH,
            "Find paths with a relative *, ?, or ** glob.",
            object_schema(
                &["root", "pattern"],
                json!({
                    "root":{"type":"string"},
                    "pattern":{"type":"string","minLength":1},
                    "max_results":{"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            (CancellationContract::BeforeEffect, RetryPolicy::Safe),
        ),
        definition(
            TEXT_SEARCH,
            "Search text files with fixed text or a regular expression.",
            object_schema(
                &["root", "query"],
                json!({
                    "root":{"type":"string"},
                    "query":{"type":"string","minLength":1},
                    "regex":{"type":"boolean"},
                    "case_sensitive":{"type":"boolean"},
                    "include":{"type":"string"},
                    "max_results":{"type":"integer","minimum":1}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::ReadRoot, ResourceClaimKind::OutputStore],
            (CancellationContract::BeforeEffect, RetryPolicy::Safe),
        ),
        definition(
            FILE_WRITE,
            "Atomically create or replace a text file with a hash precondition.",
            object_schema(
                &["path", "content", "idempotency_key"],
                json!({
                    "path":{"type":"string"},
                    "content":{"type":"string"},
                    "expected_sha256":{"type":"string"},
                    "create_only":{"type":"boolean"},
                    "idempotency_key":{"type":"string","minLength":1,"maxLength":256}
                }),
            ),
            &write_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::WorktreeWrite],
            (CancellationContract::AtomicCommit, RetryPolicy::Reconcile),
        ),
        definition(
            APPLY_PATCH,
            "Apply exact-content replacements after a full-file hash check.",
            object_schema(
                &["path", "expected_sha256", "operations", "idempotency_key"],
                json!({
                    "path":{"type":"string"},
                    "expected_sha256":{"type":"string","minLength":1},
                    "operations":{"type":"array","minItems":1,"items":{
                        "type":"object","additionalProperties":false,
                        "required":["expected","replacement"],
                        "properties":{
                            "expected":{"type":"string","minLength":1},
                            "replacement":{"type":"string"},
                            "expected_occurrences":{"type":"integer","minimum":1}
                        }
                    }},
                    "idempotency_key":{"type":"string","minLength":1,"maxLength":256}
                }),
            ),
            &write_caps,
            ToolExecutionMode::Immediate,
            &[ResourceClaimKind::WorktreeWrite],
            (CancellationContract::AtomicCommit, RetryPolicy::Reconcile),
        ),
        definition(
            PROCESS_START,
            "Start an argv or explicitly typed shell process in the platform sandbox.",
            object_schema(
                &["program", "cwd", "idempotency_key"],
                json!({
                    "program":{"type":"string","minLength":1},
                    "args":{"type":"array","items":{"type":"string"}},
                    "shell_script":{"type":"string"},
                    "cwd":{"type":"string"},
                    "environment":{"type":"object","additionalProperties":{"type":"string"}},
                    "read_only":{"type":"boolean"},
                    "network":{"type":"boolean"},
                    "outside_write":{"type":"boolean"},
                    "git_metadata_write":{"type":"boolean"},
                    "git_push_or_destructive":{"type":"boolean"},
                    "interactive":{"type":"boolean"},
                    "timeout_ms":{"type":"integer","minimum":1},
                    "idempotency_key":{"type":"string","minLength":1,"maxLength":256}
                }),
            ),
            &process_caps,
            ToolExecutionMode::BackgroundProcess,
            &[
                ResourceClaimKind::ReadRoot,
                ResourceClaimKind::WorktreeWrite,
                ResourceClaimKind::OutsideWrite,
                ResourceClaimKind::GitMetadata,
                ResourceClaimKind::Network,
                ResourceClaimKind::OutputStore,
            ],
            (
                CancellationContract::ProcessTree,
                RetryPolicy::NeverAfterStart,
            ),
        ),
        control_definition(
            PROCESS_POLL,
            "Poll a process and return only new bounded output.",
        ),
        definition(
            PROCESS_WAIT,
            "Wait up to 60 seconds for a process while keeping output responsive.",
            object_schema(
                &["handle"],
                json!({
                    "handle":{"type":"string","minLength":1},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":60000}
                }),
            ),
            &read_caps,
            ToolExecutionMode::ProcessControl,
            &[ResourceClaimKind::OutputStore],
            (CancellationContract::ProcessTree, RetryPolicy::Safe),
        ),
        definition(
            PROCESS_WRITE,
            "Write input to a running pipe or PTY and optionally close input.",
            object_schema(
                &["handle", "input"],
                json!({
                    "handle":{"type":"string","minLength":1},
                    "input":{"type":"string"},
                    "close":{"type":"boolean"}
                }),
            ),
            &read_caps,
            ToolExecutionMode::ProcessControl,
            &[ResourceClaimKind::OutputStore],
            (CancellationContract::ProcessTree, RetryPolicy::Reconcile),
        ),
        control_definition(PROCESS_TERMINATE, "Terminate and reap a process tree."),
        definition(
            OUTPUT_READ,
            "Retrieve a bounded line or byte range from a stored evidence output.",
            object_schema(
                &["id"],
                json!({
                    "id":{"type":"string","minLength":1},
                    "range":{"type":"string"},
                    "bytes":{"type":"string"}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Retrieval,
            &[ResourceClaimKind::OutputStore],
            (CancellationContract::NotApplicable, RetryPolicy::Safe),
        ),
        definition(
            MEMORY_RECALL,
            "Recall typed entries from native session, private, global, or shared memory without changing them.",
            object_schema(
                &[],
                json!({
                    "key":{"type":"string","minLength":1},
                    "scope":{"type":"string","enum":["session","private","global","shared"]}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Retrieval,
            &[ResourceClaimKind::MemoryStore],
            (CancellationContract::NotApplicable, RetryPolicy::Safe),
        ),
        definition(
            MEMORY_REMEMBER,
            "Store one explicit fact in a selected memory scope; session is the safe default.",
            object_schema(
                &["key", "text"],
                json!({
                    "key":{"type":"string","minLength":1},
                    "text":{"type":"string","minLength":1},
                    "scope":{"type":"string","enum":["session","private","global","shared"],"default":"session"}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[
                ResourceClaimKind::MemoryStore,
                ResourceClaimKind::WorktreeWrite,
            ],
            (CancellationContract::AtomicCommit, RetryPolicy::Reconcile),
        ),
        definition(
            MEMORY_FORGET,
            "Remove one fact from a selected memory scope; session is the safe default.",
            object_schema(
                &["key"],
                json!({
                    "key":{"type":"string","minLength":1},
                    "scope":{"type":"string","enum":["session","private","global","shared"],"default":"session"}
                }),
            ),
            &read_caps,
            ToolExecutionMode::Immediate,
            &[
                ResourceClaimKind::MemoryStore,
                ResourceClaimKind::WorktreeWrite,
            ],
            (CancellationContract::AtomicCommit, RetryPolicy::Reconcile),
        ),
        definition(
            CONTEXT_SEARCH,
            "Search prior sessions and bounded Zirv evidence without making a model call.",
            object_schema(&["query"], json!({"query":{"type":"string","minLength":1}})),
            &read_caps,
            ToolExecutionMode::Retrieval,
            &[ResourceClaimKind::SearchIndex],
            (CancellationContract::NotApplicable, RetryPolicy::Safe),
        ),
        definition(
            DELEGATE,
            "Start one worker on a shared task card, on the native or the legacy runtime, and              return its durable launch receipt and stable delegation handle.",
            object_schema(
                &["brief"],
                json!({
                    "brief":{"type":"string","minLength":1},
                    "target":{"type":"string","minLength":1},
                    "runtime":{"type":"string","enum":["native","harness"],"default":"native"},
                    "role":{"type":"string","minLength":1},
                    "task":{"type":"string","minLength":1},
                    "group":{"type":"string","minLength":1},
                    "workdir":{"type":"string","minLength":1},
                    "mode":{"type":"string","enum":["writing","read_only"],"default":"writing"},
                    "budget_tokens":{"type":"integer","minimum":1},
                    "max_tool_calls":{"type":"integer","minimum":1}
                }),
            ),
            &delegate_caps,
            ToolExecutionMode::BackgroundProcess,
            &[
                ResourceClaimKind::DelegationStore,
                ResourceClaimKind::WorktreeWrite,
            ],
            (
                CancellationContract::AtomicCommit,
                RetryPolicy::NeverAfterStart,
            ),
        ),
        handle_definition(
            SEND,
            "Send a directed message to a live delegated worker. A message that arrives while              the worker has an approval or other attention latch open is queued and retried at              the next idle boundary, never typed at the dialog.",
            json!({"message":{"type":"string","minLength":1}}),
            &["delegation", "message"],
            RetryPolicy::Reconcile,
        ),
        handle_definition(
            WAIT,
            "Bounded wait on one delegation's durable state. Answers from the record and a              deadline; makes no model call and wakes no worker.",
            json!({"timeout_secs":{"type":"integer","minimum":1}}),
            &["delegation"],
            RetryPolicy::Safe,
        ),
        handle_definition(
            RESULT,
            "Bounded result manifest for one delegation: outcome, delivery identities, report              reference and unknown tool outcomes. Never the worker's transcript.",
            json!({"max_bytes":{"type":"integer","minimum":256}}),
            &["delegation"],
            RetryPolicy::Safe,
        ),
        handle_definition(
            FOLLOW_UP,
            "Continue the ORIGINAL worker of one delegation: directed while it is live, a              journal resume for a finished native worker, otherwise a transparent replacement              checkpoint. Never falls back to a most-recent session.",
            json!({"message":{"type":"string","minLength":1}}),
            &["delegation", "message"],
            RetryPolicy::Reconcile,
        ),
        handle_definition(
            INTERRUPT,
            "Request cancellation of one delegation. An effect that already started stays an              unknown outcome and must be reconciled before any retry.",
            json!({}),
            &["delegation"],
            RetryPolicy::Reconcile,
        ),
        handle_definition(
            CLOSE,
            "Release one delegation's reservations and write claims and retire it, preserving              every receipt it published and every unknown tool outcome.",
            json!({}),
            &["delegation"],
            RetryPolicy::Reconcile,
        ),
    ]
}

/// The six delegation tools that address an EXISTING delegation all share one
/// shape: a validated `delegation` handle plus at most one extra field.
fn handle_definition(
    name: &str,
    description: &str,
    extra: Value,
    required: &[&str],
    retry: RetryPolicy,
) -> ToolDefinition {
    let mut properties = json!({"delegation":{"type":"string","minLength":1,"maxLength":128}});
    if let (Some(target), Some(extra)) = (properties.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    definition(
        name,
        description,
        object_schema(required, properties),
        &["tool_access", "delegation"],
        ToolExecutionMode::Immediate,
        &[ResourceClaimKind::DelegationStore],
        (CancellationContract::AtomicCommit, retry),
    )
}

fn control_definition(name: &str, description: &str) -> ToolDefinition {
    definition(
        name,
        description,
        object_schema(
            &["handle"],
            json!({"handle":{"type":"string","minLength":1}}),
        ),
        &["tool_access"],
        ToolExecutionMode::ProcessControl,
        &[ResourceClaimKind::OutputStore],
        (
            CancellationContract::ProcessTree,
            if name == PROCESS_POLL {
                RetryPolicy::Safe
            } else {
                RetryPolicy::Reconcile
            },
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 coding/knowledge tools (#474-#475) plus the 7 delegation tools
    /// (#479). Asserted as a number on purpose: a tool added without a
    /// deliberate decision here is a tool the model was handed silently.
    const NATIVE_TOOL_COUNT: usize = 23;

    #[test]
    fn registry_names_are_unique_and_schemas_are_closed_objects() {
        let registry = ToolRegistry::native();
        let names: Vec<&str> = registry
            .definitions()
            .map(|definition| definition.name.as_str())
            .collect();
        assert_eq!(names.len(), NATIVE_TOOL_COUNT);
        assert_eq!(
            names
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            NATIVE_TOOL_COUNT
        );
        for definition in registry.definitions() {
            assert_eq!(definition.input_schema["type"], "object");
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert!(!definition.capabilities.is_empty());
        }
    }

    #[test]
    fn malformed_or_incomplete_arguments_never_become_a_typed_tool() {
        let registry = ToolRegistry::native();
        let missing = registry
            .parse(FILE_READ, json!({}))
            .expect_err("path is required");
        assert_eq!(missing.code, ToolErrorCode::InvalidArguments);
        let unknown = registry
            .parse("shell_magic", json!({}))
            .expect_err("closed registry");
        assert_eq!(unknown.code, ToolErrorCode::UnknownTool);
        let extra = registry
            .parse(FILE_READ, json!({"path":"a", "surprise":true}))
            .expect_err("unknown fields are rejected");
        assert_eq!(extra.code, ToolErrorCode::InvalidArguments);
    }

    #[test]
    fn process_environment_and_argv_are_part_of_the_authorized_action() {
        let parsed = ToolRegistry::native()
            .parse(
                PROCESS_START,
                json!({
                    "program":"printf",
                    "args":["%s", "hello world"],
                    "cwd":".",
                    "environment":{"LANG":"C"},
                    "read_only":true,
                    "idempotency_key":"run-1"
                }),
            )
            .expect("parse");
        let ExecutionAction::Process {
            invocation,
            effects,
        } = parsed.action()
        else {
            panic!("process action");
        };
        let ProcessInvocation::Argv {
            program,
            args,
            environment,
            ..
        } = invocation
        else {
            panic!("argv");
        };
        assert_eq!(program, "printf");
        assert_eq!(args, ["%s", "hello world"]);
        assert_eq!(environment.get("LANG").map(String::as_str), Some("C"));
        assert!(!effects.repo_write);
    }

    #[test]
    fn knowledge_tools_have_typed_scope_and_effects() {
        let registry = ToolRegistry::native();
        for name in [
            MEMORY_RECALL,
            MEMORY_REMEMBER,
            MEMORY_FORGET,
            CONTEXT_SEARCH,
        ] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }
        let parsed = registry
            .parse(
                MEMORY_REMEMBER,
                json!({"key":"architecture", "text":"native", "scope":"shared"}),
            )
            .expect("parse memory write");
        assert_eq!(
            parsed.action(),
            ExecutionAction::Knowledge {
                service: "memory".into(),
                operation: "remember".into(),
                scope: Some("shared".into()),
                key: Some("architecture".into()),
                write: true,
            }
        );
    }

    #[test]
    fn limits_are_derived_from_operator_config() {
        let limits = ToolLimits::testing();
        assert_eq!(limits.max_processes, 4);
        assert_eq!(limits.process.max_inline_bytes, 1024);
    }

    // -- the delegation tools (issue #479, roadmap N10) -------------------

    use crate::commands::ctx::attention;
    use crate::commands::ctx::delegation as service;
    use crate::commands::ctx::runtime::enforcement::{
        ApprovalAuthority, ApprovalMode, ConfigPolicySource, ExecutionIdentity, NetworkScope,
        PlatformIsolation, ResourceClaims, StoredSeatFence,
    };
    use crate::commands::ctx::seat;

    struct DelegationFixture {
        _root: tempfile::TempDir,
        state: StateDir,
        repo: PathBuf,
        client: NativeToolClient,
        launches: std::sync::Arc<std::sync::Mutex<Vec<service::LaunchRequest>>>,
    }

    /// A real `NativeToolClient` -- real registry, real broker, real seat
    /// fence -- with only the WORKER LAUNCH replaced, so the delegation tools
    /// are exercised through their production path without starting anything.
    fn delegation_fixture(exit_code: i32) -> DelegationFixture {
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
                role: "orchestrator".to_string(),
                pinned: false,
                phase: Default::default(),
                visited: Vec::new(),
                last_rollover_at: None,
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
                role: "orchestrator".to_string(),
                task: None,
            },
            ResourceClaims::new(&repo, &repo, state.root(), &home, NetworkScope::Denied)
                .expect("claims"),
            ApprovalMode::Headless,
            std::sync::Arc::new(ConfigPolicySource::new(repo.clone())),
            std::sync::Arc::new(StoredSeatFence::new(state.clone())),
            std::sync::Arc::new(ApprovalAuthority::new()),
            None,
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

    fn call(client: &mut NativeToolClient, name: &str, arguments: Value) -> ToolReceipt {
        client.execute(name, arguments, None, None)
    }

    fn handle_from(receipt: ToolReceipt) -> String {
        receipt.result.expect("result")["delegation"]
            .as_str()
            .expect("delegation handle")
            .to_string()
    }

    #[test]
    fn every_delegation_tool_is_registered_with_a_closed_schema_and_a_delegation_claim() {
        let registry = ToolRegistry::native();
        for name in delegation::ALL {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert!(
                definition
                    .resource_claims
                    .contains(&ResourceClaimKind::DelegationStore),
                "{name} must declare the delegation store it touches"
            );
        }
        assert_eq!(
            registry.get(DELEGATE).map(|definition| definition.retry),
            Some(RetryPolicy::NeverAfterStart),
            "a dispatched worker must never be re-dispatched by a blind retry"
        );
    }

    #[test]
    fn a_native_orchestrator_delegates_to_either_runtime_through_one_service() {
        // Acceptance criteria (a) and (f): the same tool, the same durable
        // record and the same bounded manifest whichever runtime ran the
        // worker -- a mixed fleet is one ownership view, not two.
        for runtime in ["native", "harness"] {
            let mut fixture = delegation_fixture(0);
            let receipt = call(
                &mut fixture.client,
                DELEGATE,
                json!({
                    "brief": "read src/main.rs and report the entry point",
                    "runtime": runtime,
                    "target": "claude",
                    "task": "task-7",
                }),
            );
            assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
            let result = receipt.result.clone().expect("result");
            assert_eq!(result["runtime"], runtime);
            assert_eq!(result["task"], "task-7");
            assert_eq!(result["phase"], "completed");
            let handle = handle_from(receipt);
            assert!(
                result["delivery"]
                    .as_str()
                    .is_some_and(|identity| identity.starts_with(&handle)),
                "the terminal outcome carries a delivery identity"
            );
            assert_eq!(
                fixture.launches.lock().expect("launches").len(),
                1,
                "exactly one worker was started"
            );

            // The bounded manifest an unchanged orchestrator consumes.
            let manifest = call(
                &mut fixture.client,
                RESULT,
                json!({ "delegation": handle.clone() }),
            )
            .result
            .expect("manifest");
            assert_eq!(manifest["phase"], "completed");
            assert_eq!(manifest["runtime"], runtime);
            assert_eq!(manifest["deliveries"].as_array().map(Vec::len), Some(1));

            // And the durable record behind it says the same thing.
            let record = service::load(&fixture.state, &fixture.repo, &handle).expect("record");
            assert_eq!(record.handle.task.as_deref(), Some("task-7"));
            assert_eq!(record.attempts.len(), 1);
        }
    }

    #[test]
    fn a_failed_worker_is_never_reported_as_a_completed_delegation() {
        let mut fixture = delegation_fixture(2);
        let result = call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief": "do the thing"}),
        )
        .result
        .expect("result");
        assert_eq!(result["phase"], "failed");
        assert_eq!(result["exit_code"], 2);
    }

    #[test]
    fn a_message_to_a_worker_with_an_approval_open_is_queued_not_typed() {
        // Acceptance criterion (f): #468's rule, reached through the native
        // tool surface rather than the dashboard pane sweep.
        let mut fixture = delegation_fixture(0);
        let handle = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief": "investigate"}),
        ));

        let record = service::load(&fixture.state, &fixture.repo, &handle).expect("record");
        attention::record(
            &fixture.state,
            &record.handle.short,
            attention::Observation::new(
                attention::Authority::AdapterHook,
                "permission prompt open",
                90,
                1,
            )
            .with_attention(attention::Attention::Approval),
            1,
        );

        let queued = call(
            &mut fixture.client,
            SEND,
            json!({"delegation": handle, "message": "status?"}),
        )
        .result
        .expect("result");
        assert_eq!(queued["delivered"], false);
        assert_eq!(queued["reason"], "approval-open");
    }

    #[test]
    fn follow_up_interrupt_and_close_all_address_the_original_delegation() {
        // Acceptance criteria (d) and (e), through the tools.
        let mut fixture = delegation_fixture(0);
        let handle = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief": "look"}),
        ));

        let follow_up = call(
            &mut fixture.client,
            FOLLOW_UP,
            json!({"delegation": handle.clone(), "message": "and the tests?"}),
        )
        .result
        .expect("result");
        assert_eq!(follow_up["route"], "resume");
        assert_eq!(follow_up["attempt"], 2);

        let unknown = call(
            &mut fixture.client,
            FOLLOW_UP,
            json!({"delegation": "nosuchdelegation", "message": "hi"}),
        );
        assert_eq!(
            unknown.state,
            ToolReceiptState::Failed,
            "an unknown handle must fail, never fall back to a recent session"
        );

        let interrupted = call(
            &mut fixture.client,
            INTERRUPT,
            json!({ "delegation": handle.clone() }),
        )
        .result
        .expect("result");
        assert_eq!(interrupted["phase"], "cancelled");

        let closed = call(&mut fixture.client, CLOSE, json!({ "delegation": handle }))
            .result
            .expect("result");
        assert_eq!(closed["phase"], "closed");
        assert!(
            closed["receipts"]
                .as_array()
                .is_some_and(|receipts| !receipts.is_empty()),
            "closing preserves the receipts already published"
        );
    }

    #[test]
    fn a_bounded_wait_on_a_live_delegation_reports_pending_without_waking_anything() {
        let mut fixture = delegation_fixture(0);
        let handle = service::record_launch(
            &fixture.state,
            &fixture.repo,
            service::WorkerHandle {
                delegation: "livedelegation".to_string(),
                attempt: 1,
                runtime: super::super::RuntimeKind::Native,
                worker_session: "w".to_string(),
                short: "wshort".to_string(),
                role: "worker".to_string(),
                task: None,
                group: None,
                objective: None,
                workdir: fixture.repo.clone(),
            },
            None,
            1,
        )
        .expect("launch")
        .handle
        .delegation;
        let waited = call(
            &mut fixture.client,
            WAIT,
            json!({"delegation": handle, "timeout_secs": 99999}),
        )
        .result
        .expect("result");
        assert_eq!(waited["ready"], false);
        assert_eq!(waited["deadline_secs"], delegation::MAX_WAIT_SECS);
    }

    #[test]
    fn a_malformed_handle_from_provider_output_never_reaches_the_store() {
        let mut fixture = delegation_fixture(0);
        let receipt = call(
            &mut fixture.client,
            RESULT,
            json!({"delegation": "../../etc/passwd"}),
        );
        assert_eq!(receipt.state, ToolReceiptState::Failed);
        assert_eq!(
            receipt.error.map(|error| error.code),
            Some(ToolErrorCode::InvalidArguments)
        );
    }
}
