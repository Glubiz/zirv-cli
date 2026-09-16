//! Provider-owned agent loops. Never a token-level ProviderAdapter.
//! Only the official process sees its login. Tool effects cross the existing
//! Zirv broker via MCP; stdout tool events are strictly observations.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use super::super::provider::adapter::{Cancellation, ProviderUsage};
use super::super::{
    CtxResult,
    config::EnvLookup,
    provider::{RouteId, config::NativeConfig},
    state, supervise,
};
use super::journal::RouteIdentity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CONTRACT_VERSION: u32 = 1;
const MAX_RECORD: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
const MAX_PROMPT: usize = 1024 * 1024;
const BRIDGE_ADDRESS: &str = "ZIRV_EXECUTION_BRIDGE_ADDRESS";
const BRIDGE_SECRET: &str = "ZIRV_EXECUTION_BRIDGE_SECRET";

/// Registry entry: adding another provider harness requires an adapter plus
/// its independently reviewed provider/auth/billing contract, not changes to
/// native UI, tools, persistence or direct HTTP transports.
#[derive(Debug)]
pub struct AdapterSpec {
    pub id: &'static str,
    pub provider: &'static str,
    pub vendor: &'static str,
    pub harness: &'static str,
    pub shared_pool: &'static str,
    pub authentication_owner: &'static str,
}

pub fn spec(id: &str) -> CtxResult<&'static AdapterSpec> {
    static CLAUDE: AdapterSpec = AdapterSpec {
        id: "claude-code",
        provider: "anthropic",
        vendor: "anthropic",
        harness: "claude",
        shared_pool: "anthropic",
        authentication_owner: "official-harness",
    };
    match id {
        "claude-code" => Ok(&CLAUDE),
        _ => Err("runtime capability unavailable: unknown execution adapter".into()),
    }
}

pub fn discover(
    config: &ExecutionConfig,
    repo: &Path,
    home: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<Box<dyn ExecutionAdapter>> {
    match spec(&config.adapter)?.id {
        "claude-code" => Ok(Box::new(ClaudeCode::discover(config, repo, home, env)?)),
        _ => Err("runtime capability unavailable".into()),
    }
}

pub fn create(
    config: &ExecutionConfig,
    repo: &Path,
    home: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<Box<dyn ExecutionAdapter>> {
    let adapter = discover(config, repo, home, env)?;
    adapter.verify_auth()?;
    Ok(adapter)
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    pub adapter: String,
    #[serde(default)]
    pub program: Option<PathBuf>,
}

impl ExecutionConfig {
    pub fn validate(&self, config: &NativeConfig, id: &RouteId) -> CtxResult<()> {
        let spec = spec(&self.adapter)?;
        let route = &config.routes[id];
        let account = config
            .accounts
            .get(&route.account)
            .ok_or("execution account missing")?;
        if account.provider.as_ref() != spec.provider {
            return Err(
                format!("{} execution requires provider={}", spec.id, spec.provider).into(),
            );
        }
        if account.credential.is_some() {
            return Err("remove the execution credential from native.toml; use `zirv ctx provider login <route>` to authenticate in official Claude Code; tokens are never imported".into());
        }
        if route.endpoint.is_some() || !route.extensions.is_empty() || route.deployment.is_some() {
            return Err("provider execution cannot select an API endpoint, deployment or request extensions".into());
        }
        if self
            .program
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(
                "execution.program must be an absolute path to the official installation".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionCapabilities {
    pub version: u32,
    pub start: bool,
    pub follow_up: bool,
    pub interrupt: bool,
    pub exact_resume: bool,
    pub mid_turn_steering: bool,
    pub approvals: &'static str,
    pub internal_subagents: bool,
}

pub fn capabilities() -> ExecutionCapabilities {
    ExecutionCapabilities {
        version: CONTRACT_VERSION,
        start: true,
        follow_up: true,
        interrupt: true,
        exact_resume: true,
        mid_turn_steering: false,
        approvals: "zirv-mcp-broker",
        internal_subagents: false,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub backend: &'static str,
    pub authentication_owner: &'static str,
    pub billing: &'static str,
    pub authentication: Authentication,
    pub version: Option<String>,
    pub state: String,
    pub capabilities: ExecutionCapabilities,
    pub billed_spend: Option<f64>,
    pub allowance_remaining: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecutionObservation {
    pub backend: String,
    pub authentication_owner: &'static str,
    pub billing: &'static str,
    pub authentication: Authentication,
    pub adapter_version: u32,
    pub installed_version: Option<String>,
    pub estimated_api_cost: Option<f64>,
    pub billed_spend: Option<f64>,
    pub allowance_remaining: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum ExecutionEvent {
    Initialized {
        session: String,
        model: String,
    },
    Text(String),
    ToolObserved {
        id: String,
        parent: Option<String>,
        name: String,
    },
    ToolRequest {
        name: String,
        arguments: Value,
    },
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub text: String,
    pub usage: ProviderUsage,
    pub estimated_api_cost: Option<f64>,
}

pub struct ExecutionRequest<'a> {
    pub session: &'a str,
    pub resume: bool,
    pub prompt: &'a str,
    pub system: &'a str,
    pub model: &'a str,
    pub tools: Value,
    pub max_turns: u32,
    pub timeout: Duration,
    pub idle_timeout: Duration,
    pub cancel: &'a dyn Cancellation,
}

/// Future vendor adapters implement execution and status without changing
/// provider clients or importing a vendor's credentials into Zirv.
pub trait ExecutionAdapter: std::fmt::Debug + Send {
    fn id(&self) -> &str;
    fn verify_auth(&self) -> CtxResult<()>;
    fn login(&self, _args: &[String]) -> CtxResult<i32> {
        Err("official login handoff unavailable for this execution adapter".into())
    }
    fn authentication(&self) -> Authentication {
        Authentication::default()
    }
    fn installed_version(&self) -> Option<&str> {
        None
    }
    fn run(
        &self,
        request: &ExecutionRequest<'_>,
        emit: &mut dyn FnMut(ExecutionEvent) -> CtxResult<Value>,
    ) -> CtxResult<ExecutionResult>;
    fn diagnostic(&self) -> Diagnostic;
}

pub struct ClaudeCode {
    program: PathBuf,
    config_directory: PathBuf,
    auth: RefCell<Authentication>,
    policy: PolicyRoots,
    directory: PathBuf,
    environment: BTreeMap<String, String>,
    version: String,
}

// Only startup controls that can defeat the native effect boundary are rejected.
// Authentication selection belongs to the official binary, not this list.
const SELECTORS: &[&str] = &[
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_SETTINGS_PATH",
    "CLAUDE_CODE_MANAGED_SETTINGS_PATH",
];

const AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_CONFIG_DIR",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_API_KEY_HELPER",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_SKIP_MANTLE_AUTH",
    "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH",
    "CLAUDE_CODE_CLIENT_CERT",
    "CLAUDE_CODE_CLIENT_KEY",
    "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_SDK_LOAD_CONFIG",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_BEARER_TOKEN_BEDROCK_MANTLE",
    "AWS_ENDPOINT_URL_BEDROCK_RUNTIME",
    "AWS_CA_BUNDLE",
    "AWS_EC2_METADATA_DISABLED",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_QUOTA_PROJECT",
    "CLOUDSDK_CONFIG",
    "CLOUDSDK_AUTH_ACCESS_TOKEN",
    "CLOUD_ML_REGION",
    "AZURE_CLIENT_ID",
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_SECRET",
    "AZURE_CLIENT_CERTIFICATE_PATH",
    "AZURE_CLIENT_CERTIFICATE_PASSWORD",
    "AZURE_FEDERATED_TOKEN_FILE",
    "AZURE_AUTHORITY_HOST",
    "AZURE_CONFIG_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

const AUTH_SETTINGS: &[&str] = &[
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "forceLoginMethod",
    "forceLoginOrgUUID",
];

pub fn check_selectors(env: EnvLookup<'_>) -> CtxResult<()> {
    for key in SELECTORS {
        if env(key).is_some_and(|v| !v.is_empty()) {
            return Err(format!("runtime capability unavailable: {key} conflicts with native startup isolation (value withheld)").into());
        }
    }
    Ok(())
}

fn environment(env: EnvLookup<'_>) -> BTreeMap<String, String> {
    [
        "HOME",
        "USERPROFILE",
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "TMPDIR",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
    ]
    .into_iter()
    .chain(AUTH_ENV.iter().copied())
    .filter_map(|key| env(key).map(|v| (key.to_string(), v)))
    .collect()
}

// Debug output must never include inherited credentials or helper commands.
impl std::fmt::Debug for ClaudeCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCode")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Authentication {
    pub method: &'static str,
    pub provider: &'static str,
    pub billing: &'static str,
}
impl Default for Authentication {
    fn default() -> Self {
        Self {
            method: "unknown",
            provider: "unknown",
            billing: "unknown",
        }
    }
}

fn authentication(status: &Value) -> Authentication {
    let method = match status["authMethod"].as_str() {
        Some("claude.ai") => "claude.ai",
        Some("api_key") => "api_key",
        Some("oauth") => "oauth",
        Some("auth_token") => "auth_token",
        _ => "unknown",
    };
    let provider = match status["apiProvider"].as_str() {
        Some("firstParty") => "firstParty",
        Some("bedrock") => "bedrock",
        Some("vertex") => "vertex",
        Some("foundry") => "foundry",
        Some("mantle") => "mantle",
        _ => "unknown",
    };
    let billing = match (provider, method) {
        ("bedrock" | "vertex" | "foundry" | "mantle", _) => "api",
        ("firstParty", "claude.ai") => "subscription",
        ("firstParty", "api_key" | "oauth" | "auth_token") => "api",
        _ => "unknown",
    };
    Authentication {
        method,
        provider,
        billing,
    }
}

fn bounded_json_file(path: &Path) -> CtxResult<Option<Value>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("auth status unknown: cannot inspect public Claude settings".into()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_RECORD + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD {
        return Err("Claude settings exceed inspection bound".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "auth status unknown: malformed public Claude settings".into())
}

fn exists_checked(path: &Path) -> CtxResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => {
            Err("runtime capability unavailable: managed startup policy cannot be inspected".into())
        }
    }
}

#[derive(Debug, Default)]
struct PolicyRoots {
    preferences: Vec<PathBuf>,
    managed: Vec<PathBuf>,
}
impl PolicyRoots {
    fn system(home: &Path) -> Self {
        Self {
            preferences: vec![
                PathBuf::from("/Library/Managed Preferences"),
                home.join("Library/Managed Preferences"),
            ],
            managed: vec![
                PathBuf::from("/Library/Application Support/ClaudeCode"),
                PathBuf::from("/etc/claude-code"),
            ],
        }
    }
}

fn check_settings(config_directory: &Path, policy: &PolicyRoots) -> CtxResult<()> {
    if exists_checked(&config_directory.join("remote-settings.json"))? {
        return Err("runtime capability unavailable: cached managed Claude policy requires effective-policy verification".into());
    }
    // macOS profiles can deliver the same managed keys without a JSON file.
    // Inspect domain presence only, including per-user managed preferences.
    for root in &policy.preferences {
        if exists_checked(root)? {
            if exists_checked(&root.join("com.anthropic.claudecode.plist"))? {
                return Err("runtime capability unavailable: macOS managed Claude preferences require policy verification".into());
            }
            for entry in std::fs::read_dir(root)? {
                let path = entry?.path();
                if path.is_dir() && exists_checked(&path.join("com.anthropic.claudecode.plist"))? {
                    return Err("runtime capability unavailable: macOS managed Claude preferences require policy verification".into());
                }
            }
        }
    }

    // Managed settings outrank invocation settings. Until a supported public
    // effective-policy interface can attest them, do not guess or override.
    for root in &policy.managed {
        for name in [
            "managed-settings.json",
            "managed-settings.d",
            "managed-mcp.json",
        ] {
            if exists_checked(&root.join(name))? {
                return Err("runtime capability unavailable: managed Claude startup policy needs effective-policy verification".into());
            }
        }
    }
    Ok(())
}

fn locate(config: &ExecutionConfig, env: EnvLookup<'_>, repo: &Path) -> CtxResult<PathBuf> {
    let chosen = match &config.program {
        Some(path) => path.clone(),
        None => {
            let path = env("PATH").unwrap_or_default();
            let names: &[&str] = if cfg!(windows) {
                &["claude.exe"]
            } else {
                &["claude"]
            };
            std::env::split_paths(&path).filter(|p| p.is_absolute()).flat_map(|p| names.iter().map(move |name| p.join(name)))
                .find(|p| p.is_file()).ok_or("binary missing: install official Claude Code or set execution.program to its absolute path")?
        }
    };
    let path = chosen
        .canonicalize()
        .map_err(|_| "binary missing: execution.program does not exist")?;
    if repo.canonicalize().is_ok_and(|repo| path.starts_with(repo)) {
        return Err("execution program cannot be supplied by the repository".into());
    }
    let resolved = super::super::adapters::resolve_program(&path.to_string_lossy())?;
    if !resolved.prefix.is_empty() {
        return Err("runtime capability unavailable: install the native Claude Code executable; shell shims are unsupported for this adapter".into());
    }
    Ok(path)
}

impl ClaudeCode {
    pub fn discover(
        config: &ExecutionConfig,
        repo: &Path,
        home: &Path,
        env: EnvLookup<'_>,
    ) -> CtxResult<Self> {
        if std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
        {
            return Err("runtime capability unavailable: WSL inherited managed policy verification is not implemented".into());
        }
        if cfg!(windows) {
            return Err("runtime capability unavailable: Windows managed registry policy verification is not implemented for provider execution".into());
        }
        check_selectors(env)?;
        let policy = PolicyRoots::system(home);
        let config_directory = env("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        if !config_directory.is_absolute() {
            return Err("CLAUDE_CONFIG_DIR must be an absolute user-owned path".into());
        }
        for path in [&config_directory, &config_directory.join("settings.json")] {
            if let (Ok(path), Ok(repo)) = (path.canonicalize(), repo.canonicalize())
                && path.starts_with(repo)
            {
                return Err(
                    "Claude user authentication settings cannot come from the repository".into(),
                );
            }
        }
        check_settings(&config_directory, &policy)?;
        let program = locate(config, env, repo)?;
        let directory = state::StateDir::resolve(env)?
            .root()
            .join("provider-execution");
        state::create_private_dir_all(&directory)?;
        let mut adapter = Self {
            program,
            config_directory,
            auth: RefCell::default(),
            policy,
            directory,
            environment: environment(env),
            version: String::new(),
        };
        let output = adapter.probe(&["--version"])?;
        let version =
            String::from_utf8(output).map_err(|_| "version unsupported: invalid output")?;
        let numbers: Vec<u32> = version
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| "version unsupported: expected official Claude Code version")?;
        if numbers.len() != 3 || numbers[0] != 2 || numbers[1] != 1 || numbers[2] < 248 {
            return Err("version unsupported: provider execution requires Claude Code 2.1.248 or newer in the 2.1 series".into());
        }
        adapter.version = numbers
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".");
        let help = String::from_utf8(adapter.probe(&["--help"])?)
            .map_err(|_| "invalid capability output")?;
        for flag in [
            "--restricted",
            "--setting-sources",
            "--strict-mcp-config",
            "--tools",
            "--include-partial-messages",
            "--resume",
            "--session-id",
        ] {
            if !help.contains(flag) {
                return Err(format!(
                    "runtime capability unavailable: installed Claude Code lacks {flag}"
                )
                .into());
            }
        }
        Ok(adapter)
    }

    fn command(&self) -> CtxResult<Command> {
        let mut command = Command::new(&self.program);
        command
            .current_dir(&self.directory)
            .env_clear()
            .envs(&self.environment);
        // Public user settings only. Never read Claude's credential stores.
        if let Some(settings) = self.public_settings()?
            && let Some(values) = settings.get("env").and_then(Value::as_object)
        {
            check_selectors(&|key| values.get(key).and_then(Value::as_str).map(str::to_string))?;
            // Forward authentication environment without copying secrets to a file.
            for key in AUTH_ENV {
                if let Some(value) = values.get(*key).and_then(Value::as_str) {
                    command.env(key, value);
                }
            }
        }
        Ok(command)
    }

    fn public_settings(&self) -> CtxResult<Option<Value>> {
        bounded_json_file(&self.config_directory.join("settings.json"))
    }

    fn execution_settings(&self) -> CtxResult<Value> {
        let mut settings =
            json!({"disableAllHooks":true,"enabledPlugins":{},"autoMemoryEnabled":false});
        if let Some(user) = self.public_settings()? {
            for key in AUTH_SETTINGS {
                if let Some(value) = user.get(*key) {
                    settings[*key] = value.clone();
                }
            }
        }
        Ok(settings)
    }

    fn probe(&self, args: &[&str]) -> CtxResult<Vec<u8>> {
        let mut command = self.command()?;
        command.args(args);
        capture(command, Duration::from_secs(10))
    }

    pub fn login(&self, args: &[String]) -> CtxResult<i32> {
        // Direct terminal handoff: no URL/code/credential interception or log.
        let status = self
            .command()?
            .args(["auth", "login"])
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        Ok(status.code().unwrap_or(1))
    }

    pub fn auth_status(&self) -> CtxResult<()> {
        *self.auth.borrow_mut() = Authentication::default();
        let bytes = self.probe(&["auth", "status"])?;
        let status: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "auth status unknown: unsupported public auth status response")?;
        validate_auth_status(&status)?;
        *self.auth.borrow_mut() = authentication(&status);
        Ok(())
    }
}

pub fn validate_auth_status(status: &Value) -> CtxResult<()> {
    if status.get("loggedIn").and_then(Value::as_bool) != Some(true) {
        return Err(
            "login needed: run `zirv ctx provider login <route>` (official Claude Code login)"
                .into(),
        );
    }
    Ok(())
}

fn capture(mut command: Command, timeout: Duration) -> CtxResult<Vec<u8>> {
    supervise::isolate_process_tree(&mut command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "binary missing: could not launch official Claude Code")?;
    let _guard = supervise::ChildGuard::adopt(Some(child.id()));
    let stdout = child.stdout.take().ok_or("probe stdout unavailable")?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((MAX_RECORD + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            supervise::terminate(&mut child, Duration::from_millis(200))?;
            return Err("auth status unknown: official CLI probe timed out".into());
        }
        if let Ok(result) = rx.try_recv() {
            let bytes = result?;
            if bytes.len() > MAX_RECORD {
                supervise::terminate(&mut child, Duration::from_millis(200))?;
                return Err("official CLI probe exceeded output bound".into());
            }
            // stdout EOF is not permission to wait forever for the process.
            while Instant::now() < deadline {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err("login needed or official CLI status unavailable; run official Claude Code auth login/status".into());
                    }
                    return Ok(bytes);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bounded bytes first, UTF-8 only after a complete NDJSON record. Split
/// multibyte code points are legal; malformed JSON/UTF-8 never become tools.
#[derive(Default)]
pub struct Ndjson {
    pending: Vec<u8>,
    total: usize,
}
impl Ndjson {
    pub fn push(&mut self, bytes: &[u8]) -> CtxResult<Vec<Value>> {
        self.total = self.total.saturating_add(bytes.len());
        if self.total > MAX_OUTPUT {
            return Err("protocol failure: output limit".into());
        }
        let mut values = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                if !self.pending.iter().all(u8::is_ascii_whitespace) {
                    values.push(
                        serde_json::from_slice(&self.pending)
                            .map_err(|_| "protocol failure: malformed NDJSON")?,
                    );
                }
                self.pending.clear();
            } else {
                if self.pending.len() >= MAX_RECORD {
                    return Err("protocol failure: record limit".into());
                }
                self.pending.push(*byte);
            }
        }
        Ok(values)
    }
    pub fn finish(&self) -> CtxResult<()> {
        if self.pending.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err("protocol failure: truncated NDJSON".into())
        }
    }
}

fn upstream_failure(category: &str) -> super::super::provider::adapter::ProviderFailure {
    use super::super::provider::adapter::{
        FailureClass, FailureScope, FailureScopeKind, ProviderFailure,
    };
    let (class, scope, message) = match category {
        "authentication_failed" | "oauth_org_not_allowed" => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            "authentication failure: sign in with official Claude Code",
        ),
        "account_on_hold" | "billing_error" => (
            FailureClass::Entitlement,
            FailureScopeKind::Account,
            "billing or entitlement failure: resolve the account in official Claude Code; no API fallback",
        ),
        "rate_limit" => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            "provider usage limit reached: wait for capacity or explicitly select an authorized route; no API fallback",
        ),
        "model_not_found" => (
            FailureClass::ModelAccess,
            FailureScopeKind::Model,
            "model unavailable for the effective Claude Code account",
        ),
        "overloaded" => (
            FailureClass::Overloaded,
            FailureScopeKind::Endpoint,
            "upstream failure: official Claude Code reports overload",
        ),
        _ => (
            FailureClass::Provider,
            FailureScopeKind::Endpoint,
            "upstream failure: official Claude Code did not complete the turn; no automatic replay or API fallback",
        ),
    };
    ProviderFailure::new(
        class,
        FailureScope {
            kind: scope,
            id: None,
        },
        message,
    )
}

/// No stdout/stderr is copied to diagnostics. Only typed observations escape.
fn normalize(
    value: &Value,
    expected_session: &str,
    emit: &mut dyn FnMut(ExecutionEvent) -> CtxResult<Value>,
) -> CtxResult<Option<ExecutionResult>> {
    if let Some(session) = value.get("session_id").and_then(Value::as_str)
        && session != expected_session
    {
        return Err("protocol failure: unexpected Claude session".into());
    }
    match value.get("type").and_then(Value::as_str) {
        Some("system") if value["subtype"] == "api_retry" => {
            let category = value["error"].as_str().unwrap_or("unknown");
            if matches!(
                category,
                "authentication_failed"
                    | "oauth_org_not_allowed"
                    | "account_on_hold"
                    | "billing_error"
                    | "rate_limit"
                    | "model_not_found"
            ) {
                return Err(upstream_failure(category).into());
            }
        }
        Some("rate_limit_event") if value["rate_limit_info"]["status"] == "rejected" => {
            return Err(upstream_failure("rate_limit").into());
        }
        Some("system") if value["subtype"] == "init" => {
            if value["session_id"].as_str() != Some(expected_session) {
                return Err(
                    "protocol failure: initialization lacks the exact session reference".into(),
                );
            }
            if value["tools"].as_array().is_none_or(|tools| {
                tools.iter().any(|tool| {
                    tool.as_str().is_none_or(|name| {
                        !name.starts_with("mcp__zirv__") && name != "EndConversation"
                    })
                })
            }) {
                return Err("runtime capability unavailable: Claude Code did not enforce the restricted tool set".into());
            }
            if !value["mcp_servers"].as_array().is_some_and(|servers| {
                servers
                    .iter()
                    .any(|server| server["name"] == "zirv" && server["status"] == "connected")
            }) {
                return Err(
                    "runtime capability unavailable: Zirv MCP bridge did not connect".into(),
                );
            }
            emit(ExecutionEvent::Initialized {
                session: expected_session.to_string(),
                model: value["model"].as_str().unwrap_or("").to_string(),
            })?;
        }
        Some("stream_event") if value["event"]["delta"]["type"] == "text_delta" => {
            if let Some(text) = value["event"]["delta"]["text"].as_str() {
                emit(ExecutionEvent::Text(text.to_string()))?;
            }
        }
        Some("assistant") => {
            if let Some(blocks) = value["message"]["content"].as_array() {
                for block in blocks.iter().filter(|block| block["type"] == "tool_use") {
                    emit(ExecutionEvent::ToolObserved {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        parent: value["parent_tool_use_id"].as_str().map(str::to_string),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                    })?;
                }
            }
        }
        Some("result") => {
            if value["session_id"].as_str() != Some(expected_session) {
                return Err(
                    "protocol failure: terminal result lacks the exact session reference".into(),
                );
            }
            if value["permission_denials"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
            {
                return Err("approval blocked: Claude Code denied a tool; resolve the permission in the official CLI or use the Zirv MCP tool".into());
            }
            if value["is_error"] != false || value["subtype"] != "success" {
                return Err(upstream_failure(value["error"].as_str().unwrap_or("unknown")).into());
            }
            let usage = &value["usage"];
            return Ok(Some(ExecutionResult {
                text: value["result"].as_str().unwrap_or("").to_string(),
                usage: ProviderUsage {
                    input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                    cache_creation_input_tokens: usage["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                    cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
                    ..Default::default()
                },
                estimated_api_cost: value["total_cost_usd"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0),
            }));
        }
        _ => {}
    }
    Ok(None)
}

struct EphemeralFile(PathBuf);
impl Drop for EphemeralFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct BridgeCall {
    value: Value,
    reply: mpsc::SyncSender<Value>,
}
struct Bridge {
    address: String,
    secret: String,
    calls: mpsc::Receiver<BridgeCall>,
    stop: Arc<AtomicBool>,
    seen_tools: std::collections::BTreeSet<String>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
impl Bridge {
    fn start() -> CtxResult<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?.to_string();
        let secret = uuid::Uuid::new_v4().to_string();
        let expected = secret.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (tx, calls) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                // BSD/macOS accepted sockets can inherit the listener's
                // nonblocking flag. Framed reads below must wait for the rest
                // of a record (within the timeout), not drop a partial write.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let Ok(value) = read_record(&mut BufReader::new(&mut stream)) else {
                    continue;
                };
                if value["secret"].as_str() != Some(expected.as_str()) {
                    continue;
                }
                let (reply, rx) = mpsc::sync_channel(1);
                if tx
                    .try_send(BridgeCall {
                        value: value["request"].clone(),
                        reply,
                    })
                    .is_err()
                {
                    continue;
                }
                while !stopping.load(Ordering::Acquire) {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(value) => {
                            let _ = writeln!(stream, "{value}");
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    }
                }
            }
        });
        Ok(Self {
            address,
            secret,
            calls,
            stop,
            seen_tools: Default::default(),
        })
    }

    fn serve(
        &mut self,
        tools: &Value,
        emit: &mut dyn FnMut(ExecutionEvent) -> CtxResult<Value>,
    ) -> CtxResult<()> {
        while let Ok(call) = self.calls.try_recv() {
            let id = call.value["id"].clone();
            let result = match call.value["method"].as_str() {
                Some("initialize") => {
                    json!({"protocolVersion":"2024-11-05", "capabilities":{"tools":{}},"serverInfo":{"name":"zirv","version":"1"}})
                }
                Some("ping") => json!({}),
                Some("tools/list") => json!({"tools":tools}),
                Some("tools/call") => {
                    let key = id.to_string();
                    if key.len() > 256
                        || !(id.is_string() || id.is_number())
                        || self.seen_tools.len() >= 4096
                        || !self.seen_tools.insert(key)
                    {
                        let _=call.reply.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"duplicate or invalid tool request; inspect the prior receipt, no effect replayed"}}));
                        continue;
                    }
                    let params = &call.value["params"];
                    let name = params["name"]
                        .as_str()
                        .ok_or("protocol failure: MCP tool name missing")?;
                    if !tools
                        .as_array()
                        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name))
                    {
                        json!({"content":[{"type":"text","text":"Tool unavailable"}],"isError":true})
                    } else {
                        emit(ExecutionEvent::ToolRequest {
                            name: name.to_string(),
                            arguments: params["arguments"].clone(),
                        })?
                    }
                }
                _ => {
                    let _ = call.reply.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method unavailable"}}));
                    continue;
                }
            };
            let _ = call
                .reply
                .send(json!({"jsonrpc":"2.0","id":id,"result":result}));
        }
        Ok(())
    }
}

fn read_record(reader: &mut impl BufRead) -> CtxResult<Value> {
    let mut line = Vec::new();
    reader
        .take((MAX_RECORD + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if line.len() > MAX_RECORD || !line.ends_with(b"\n") {
        return Err("MCP protocol record incomplete or too large".into());
    }
    serde_json::from_slice(&line).map_err(|_| "MCP protocol record invalid".into())
}

/// Private stdio MCP relay. It receives no provider credentials and only
/// reaches a per-turn loopback listener with an unguessable ephemeral secret.
/// The parent, never this relay, owns tool execution and approval state.
pub fn bridge_stdio() -> CtxResult<i32> {
    super::require_native_available()?;
    let address: std::net::SocketAddr = std::env::var(BRIDGE_ADDRESS)
        .map_err(|_| "bridge address missing")?
        .parse()?;
    if !address.ip().is_loopback() {
        return Err("bridge requires loopback".into());
    }
    let secret = std::env::var(BRIDGE_SECRET).map_err(|_| "bridge authorization missing")?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        if input.fill_buf()?.is_empty() {
            return Ok(0);
        }
        let request = read_record(&mut input)?;
        if request.get("id").is_none() {
            continue;
        }
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        // Approval wait is owned and cancellable in the parent. Closing the
        // parent ends this socket; no polling model requests are involved.
        writeln!(stream, "{}", json!({"secret":secret,"request":request}))?;
        let response = read_record(&mut BufReader::new(stream))?;
        writeln!(output, "{response}")?;
        output.flush()?;
    }
}

impl ExecutionAdapter for ClaudeCode {
    fn verify_auth(&self) -> CtxResult<()> {
        self.auth_status()
    }
    fn login(&self, args: &[String]) -> CtxResult<i32> {
        ClaudeCode::login(self, args)
    }
    fn authentication(&self) -> Authentication {
        *self.auth.borrow()
    }
    fn installed_version(&self) -> Option<&str> {
        Some(&self.version)
    }
    fn id(&self) -> &str {
        "claude-code"
    }
    fn diagnostic(&self) -> Diagnostic {
        let state = self.auth_status().map_or_else(
            |e| e.to_string(),
            |_| "signed-in; model availability and billed usage unverified".to_string(),
        );
        Diagnostic {
            backend: "claude-code",
            authentication_owner: "official-harness",
            billing: self.authentication().billing,
            authentication: self.authentication(),
            version: Some(self.version.clone()),
            state,
            capabilities: capabilities(),
            billed_spend: None,
            allowance_remaining: None,
        }
    }

    fn run(
        &self,
        request: &ExecutionRequest<'_>,
        emit: &mut dyn FnMut(ExecutionEvent) -> CtxResult<Value>,
    ) -> CtxResult<ExecutionResult> {
        if request.prompt.len() > MAX_PROMPT || request.system.len() > MAX_PROMPT {
            return Err("execution prompt exceeds bound".into());
        }
        uuid::Uuid::parse_str(request.session)
            .map_err(|_| "explicit execution session must be a UUID")?;
        check_settings(&self.config_directory, &self.policy)?;
        self.auth_status()?;
        let mut bridge = Bridge::start()?;
        let directory = self.directory.join(request.session);
        state::create_private_dir_all(&directory)?;
        let mcp_file = directory.join("mcp.json");
        let _ephemeral_mcp = EphemeralFile(mcp_file.clone());
        let system_file = directory.join("instructions.txt");
        let settings_file = directory.join("settings.json");
        state::write_private(&system_file, request.system)?;
        let _ephemeral_settings = EphemeralFile(settings_file.clone());
        state::write_private(&settings_file, &self.execution_settings()?.to_string())?;
        // MCP effects have no reason to inherit the model process's credentials.
        let mut bridge_environment: BTreeMap<&str, &str> =
            AUTH_ENV.iter().map(|key| (*key, "")).collect();
        bridge_environment.insert(BRIDGE_ADDRESS, &bridge.address);
        bridge_environment.insert(BRIDGE_SECRET, &bridge.secret);
        state::write_private(&mcp_file, &json!({"mcpServers":{"zirv":{"type":"stdio","command":std::env::current_exe()?,"args":["ctx","provider","bridge"],"env":bridge_environment}}}).to_string())?;
        let mut command = self.command()?;
        command
            .current_dir(&directory)
            .args(["--restricted", "--setting-sources", "", "--settings"])
            .arg(&settings_file)
            .args([
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--tools",
                "",
                "--permission-mode",
                "dontAsk",
                "--allowedTools",
                "mcp__zirv__*",
                "--strict-mcp-config",
                "--mcp-config",
            ])
            .arg(&mcp_file)
            .args(["--append-system-prompt-file"])
            .arg(&system_file)
            .args([
                "--model",
                request.model,
                "--max-turns",
                &request.max_turns.to_string(),
            ])
            .arg(if request.resume {
                "--resume"
            } else {
                "--session-id"
            })
            .arg(request.session);
        supervise::isolate_process_tree(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| "process launch failed: official Claude Code unavailable")?;
        let _guard = supervise::ChildGuard::adopt(Some(child.id()));
        let mut stdin = child.stdin.take().ok_or("child stdin unavailable")?;
        let prompt = request.prompt.as_bytes().to_vec();
        let (input_tx, input_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = input_tx.send(stdin.write_all(&prompt).is_ok());
        });
        let stdout = child.stdout.take().ok_or("child stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("child stderr unavailable")?;
        let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>, ()>>(32);
        std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut bytes = [0; 8192];
            loop {
                match stdout.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(bytes[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Err(()));
                        break;
                    }
                }
            }
        });
        let stderr_flood = Arc::new(AtomicBool::new(false));
        let flooded = Arc::clone(&stderr_flood);
        std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut bytes = [0; 8192];
            let mut total = 0usize;
            while let Ok(n) = stderr.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n);
                if total > MAX_OUTPUT {
                    flooded.store(true, Ordering::Release);
                }
            }
        });
        let started = Instant::now();
        let mut last = Instant::now();
        let mut parser = Ndjson::default();
        let mut result = None;
        let mut input_written = false;
        let mut initialized = false;
        let outcome = (|| {
            loop {
                if request.cancel.is_cancelled() {
                    return Err("cancelled: official turn interrupted; reconcile any uncertain effects before resuming".into());
                }
                if started.elapsed() > request.timeout || last.elapsed() > request.idle_timeout {
                    return Err("process timeout: turn incomplete; no automatic replay".into());
                }
                if stderr_flood.load(Ordering::Acquire) {
                    return Err("protocol failure: stderr output limit".into());
                }
                match input_rx.try_recv() {
                    Ok(true) => input_written = true,
                    Ok(false) => {
                        return Err(
                            "process input failure: task was not delivered; turn incomplete".into(),
                        );
                    }
                    Err(_) => {}
                }
                if result.is_none() {
                    let mut active_emit = |event| {
                        let output = emit(event);
                        // Local approval/tool time is not provider idle time.
                        last = Instant::now();
                        output
                    };
                    bridge.serve(&request.tools, &mut active_emit)?;
                }
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(Ok(bytes)) => {
                        last = Instant::now();
                        for value in parser.push(&bytes)? {
                            if result.is_some() {
                                return Err("protocol failure: event after terminal result".into());
                            }
                            if value["type"] == "system" && value["subtype"] == "init" {
                                initialized = true;
                            }
                            if let Some(terminal) = normalize(&value, request.session, emit)? {
                                if result.is_some() {
                                    return Err(
                                        "protocol failure: duplicate terminal result".into()
                                    );
                                }
                                result = Some(terminal);
                            }
                        }
                    }
                    Ok(Err(())) => return Err("protocol failure: stdout read failed".into()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        if let Some(status) = child.try_wait()? {
                            parser.finish()?;
                            if !input_written
                                && input_rx.recv_timeout(Duration::from_millis(50)) != Ok(true)
                            {
                                return Err(
                                    "process input failure: task delivery unconfirmed".into()
                                );
                            }
                            if !status.success() {
                                return Err("process exit: Claude Code exited unsuccessfully; turn incomplete".into());
                            }
                            if result.is_some() && !initialized {
                                return Err(
                                    "protocol failure: missing initialization receipt".into()
                                );
                            }
                            return result.take().ok_or_else(|| "protocol failure: process exited without terminal result; turn incomplete".into());
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        })();
        // Revoke bridge authority before releasing the child/process lease.
        drop(bridge);
        if outcome.is_err() {
            let _ = supervise::terminate(&mut child, Duration::from_millis(500));
        }
        // Ephemeral bridge authorization is never retained across turns.
        let _ = std::fs::remove_file(mcp_file);
        outcome
    }
}

pub fn route_identity(config: &NativeConfig, id: &RouteId) -> CtxResult<RouteIdentity> {
    if !config.allowed_routes().contains(id) {
        return Err("execution route denied by effective repository policy".into());
    }
    let route = config.routes.get(id).ok_or("execution route missing")?;
    let execution = route
        .execution
        .as_ref()
        .ok_or("route is a direct API route")?;
    execution.validate(config, id)?;
    let account = &config.accounts[&route.account];
    let spec = spec(&execution.adapter)?;
    let endpoint = super::super::provider::EndpointId::new(spec.id)?;
    let (model, _) =
        super::super::provider::inventory::resolve_model(id, &endpoint, spec.vendor, &route.model)?;
    Ok(RouteIdentity {
        route: id.clone(),
        provider: account.provider.clone(),
        endpoint,
        account: route.account.clone(),
        billing_pool: config.account_pool(&route.account),
        protocol: super::super::provider::provider(spec.provider)
            .ok_or("execution provider unavailable")?
            .protocol,
        model,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::super::provider::adapter::CancellationFlag;
    use super::*;

    #[test]
    fn fragmented_utf8_unknown_fields_and_multiple_records() {
        let bytes = "{\"type\":\"future\",\"text\":\"æ🙂\"}\n{\"type\":\"result\",\"unknown\":1}\n"
            .as_bytes();
        for split in 0..bytes.len() {
            let mut parser = Ndjson::default();
            let mut values = parser.push(&bytes[..split]).unwrap();
            values.extend(parser.push(&bytes[split..]).unwrap());
            parser.finish().unwrap();
            assert_eq!(values.len(), 2);
            assert_eq!(values[0]["text"], "æ🙂");
        }
    }

    #[test]
    fn malformed_truncated_and_oversized_records_fail_closed() {
        assert!(Ndjson::default().push(b"{broken}\n").is_err());
        assert!(Ndjson::default().push(b"{\"x\":\"\xff\"}\n").is_err());
        let mut parser = Ndjson::default();
        parser.push(b"{\"x\":1}").unwrap();
        assert!(parser.finish().is_err());
        assert!(Ndjson::default().push(&vec![b'x'; MAX_RECORD + 1]).is_err());
    }

    #[test]
    fn conflicting_selectors_are_named_without_leaking_values() {
        for selector in SELECTORS {
            let error =
                check_selectors(&|key| (key == *selector).then(|| "SECRET-NEVER-LOG".to_string()))
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(selector));
            assert!(!error.contains("SECRET-NEVER-LOG"));
        }
        let inherited = environment(&|key| Some(format!("value-{key}")));
        assert!(inherited.contains_key("ANTHROPIC_API_KEY"));
        assert!(inherited.contains_key("AWS_PROFILE"));
        assert!(inherited.contains_key("GOOGLE_APPLICATION_CREDENTIALS"));
        assert!(inherited.contains_key("AZURE_CLIENT_ID"));
        assert!(!inherited.contains_key("OPENAI_API_KEY"));
        assert!(!inherited.contains_key("NODE_OPTIONS"));
    }

    #[test]
    fn public_status_accepts_all_official_auth_methods_and_plans() {
        for (method, provider, plan, billing) in [
            ("claude.ai", "firstParty", "free", "subscription"),
            ("claude.ai", "firstParty", "pro", "subscription"),
            ("claude.ai", "firstParty", "max", "subscription"),
            ("claude.ai", "firstParty", "team", "subscription"),
            ("claude.ai", "firstParty", "enterprise", "subscription"),
            ("api_key", "firstParty", "", "api"),
            ("oauth", "firstParty", "", "api"),
            ("unknown", "bedrock", "", "api"),
            ("unknown", "vertex", "", "api"),
            ("unknown", "foundry", "", "api"),
            ("future", "future", "", "unknown"),
        ] {
            let status = json!({"loggedIn":true,"authMethod":method,"apiProvider":provider,"subscriptionType":plan,"email":"PRIVATE"});
            validate_auth_status(&status).unwrap();
            assert_eq!(authentication(&status).billing, billing);
            assert!(
                !serde_json::to_string(&authentication(&status))
                    .unwrap()
                    .contains("PRIVATE")
            );
        }
        assert!(validate_auth_status(&json!({"loggedIn":false})).is_err());
        assert!(validate_auth_status(&json!({})).is_err());
    }

    #[test]
    fn tool_stream_observation_is_never_a_tool_request() {
        let mut observations = Vec::new();
        normalize(&json!({"type":"assistant","session_id":"session","parent_tool_use_id":"parent","message":{"content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"touch dangerous"}}]}}), "session", &mut |event| { observations.push(event); Ok(Value::Null) }).unwrap();
        assert!(
            matches!(&observations[..], [ExecutionEvent::ToolObserved { id, parent:Some(parent), name }] if id == "call" && parent == "parent" && name == "Bash")
        );
    }

    #[test]
    fn terminal_success_is_required_and_estimates_are_separate() {
        let result = normalize(&json!({"type":"result","session_id":"exact","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.07,"usage":{"input_tokens":11,"output_tokens":2}}), "exact", &mut |_| Ok(Value::Null)).unwrap().unwrap();
        assert_eq!(result.usage.input_tokens, 11);
        assert_eq!(result.estimated_api_cost, Some(0.07));
        assert!(normalize(&json!({"type":"result","session_id":"unrelated","subtype":"success","is_error":false}), "exact", &mut |_| Ok(Value::Null)).is_err());
        for value in [
            json!({"type":"result","is_error":true,"errors":["SECRET"]}),
            json!({"type":"result","permission_denials":[{"tool":"Bash"}]}),
        ] {
            let error = normalize(&value, "exact", &mut |_| Ok(Value::Null))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("SECRET"));
        }
    }

    #[test]
    fn initialization_refuses_unrestricted_tools_and_upstream_failures_are_typed() {
        let init = json!({"type":"system","subtype":"init","session_id":"exact","tools":["Bash"],"mcp_servers":[{"name":"zirv","status":"connected"}]});
        assert!(
            normalize(&init, "exact", &mut |_| Ok(Value::Null))
                .unwrap_err()
                .to_string()
                .contains("restricted tool set")
        );
        let mut disconnected = init;
        disconnected["tools"] = json!([]);
        disconnected["mcp_servers"] = json!([]);
        assert!(
            normalize(&disconnected, "exact", &mut |_| Ok(Value::Null))
                .unwrap_err()
                .to_string()
                .contains("did not connect")
        );
        for (category, class) in [
            (
                "rate_limit",
                super::super::super::provider::adapter::FailureClass::RateLimited,
            ),
            (
                "authentication_failed",
                super::super::super::provider::adapter::FailureClass::Authentication,
            ),
            (
                "model_not_found",
                super::super::super::provider::adapter::FailureClass::ModelAccess,
            ),
        ] {
            let error = normalize(
                &json!({"type":"system","subtype":"api_retry","error":category}),
                "exact",
                &mut |_| Ok(Value::Null),
            )
            .unwrap_err();
            let typed = error
                .downcast_ref::<super::super::super::provider::adapter::ProviderFailure>()
                .unwrap();
            assert_eq!(typed.class, class);
            assert!(!typed.retry.retryable);
        }
    }

    #[test]
    fn public_settings_do_not_require_reading_token_storage() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            "invalid secret storage",
        )
        .unwrap();
        std::fs::write(
            home.path().join("settings.json"),
            r#"{"apiKeyHelper":"user helper","forceLoginMethod":"console"}"#,
        )
        .unwrap();
        check_settings(home.path(), &PolicyRoots::default()).unwrap();
    }

    fn config() -> NativeConfig {
        let mut config: NativeConfig = toml::from_str(
            r#"
            schema=1
            [account.plan]
            provider='anthropic'
            billing='subscription'
            [route.claude]
            account='plan'
            model='sonnet'
            execution={adapter='claude-code'}
        "#,
        )
        .unwrap();
        config.policy.allowed_routes = Some(config.routes.keys().cloned().collect());
        config
    }

    #[test]
    fn execution_configuration_cannot_become_token_transport() {
        let mut config = config();
        let route = RouteId::new("claude").unwrap();
        route_identity(&config, &route).unwrap();
        config.accounts.values_mut().next().unwrap().credential =
            Some(super::super::super::provider::credential::CredentialRef::Env("SECRET".into()));
        let error = route_identity(&config, &route).unwrap_err().to_string();
        assert!(error.contains("tokens are never imported"));
        assert!(!error.contains("SECRET"));
    }

    #[test]
    fn aliases_share_one_subscription_pool_and_retain_billing() {
        let mut config = config();
        let account = config.accounts.values().next().unwrap().clone();
        let other = super::super::super::provider::AccountId::new("other").unwrap();
        config.accounts.insert(other.clone(), account);
        let mut route = config.routes.values().next().unwrap().clone();
        route.account = other.clone();
        config.routes.insert(RouteId::new("other").unwrap(), route);
        assert_eq!(
            config.account_pool(&other),
            config.account_pool(&super::super::super::provider::AccountId::new("plan").unwrap())
        );
        let offers = super::super::super::route::offers_from_config(&config);
        assert!(offers[0].identity.shares_pool(
            &super::super::super::route::RouteIdentity::harness("claude", "anthropic")
        ));
        assert!(
            offers
                .iter()
                .all(|offer| offer.billing
                    == super::super::super::route::BillingPosture::Subscription)
        );
        assert!(
            offers
                .iter()
                .all(|offer| offer.identity.endpoint == "claude-code")
        );
    }

    #[test]
    fn api_execution_is_billable_without_a_zirv_credential() {
        let mut config = config();
        config.accounts.values_mut().next().unwrap().billing =
            super::super::super::provider::BillingClass::Api;
        route_identity(&config, &RouteId::new("claude").unwrap()).unwrap();
        let offers = super::super::super::route::offers_from_config(&config);
        assert_eq!(
            offers[0].billing,
            super::super::super::route::BillingPosture::Api
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_auth_settings_survive_without_enabling_hooks_or_leaking_secrets() {
        let (_tmp, mut adapter) = fake_cli("exit 0");
        std::fs::create_dir_all(&adapter.config_directory).unwrap();
        std::fs::write(adapter.config_directory.join("settings.json"), r#"{
            "apiKeyHelper":"user-owned-helper", "forceLoginMethod":"console",
            "awsAuthRefresh":"aws sso login", "enabledPlugins":{"untrusted":true},
            "hooks":{"SessionStart":"unsafe"},
            "env":{"ANTHROPIC_API_KEY":"SECRET-API", "CLAUDE_CODE_USE_BEDROCK":"1", "NODE_OPTIONS":"unsafe"}
        }"#).unwrap();
        adapter
            .environment
            .insert("AZURE_CLIENT_SECRET".into(), "SECRET-AZURE".into());
        let settings = adapter.execution_settings().unwrap();
        assert_eq!(settings["apiKeyHelper"], "user-owned-helper");
        assert_eq!(settings["forceLoginMethod"], "console");
        assert_eq!(settings["awsAuthRefresh"], "aws sso login");
        assert_eq!(settings["disableAllHooks"], true);
        assert_eq!(settings["enabledPlugins"], json!({}));
        assert!(settings.get("hooks").is_none());
        assert!(!settings.to_string().contains("SECRET"));
        let command = adapter.command().unwrap();
        let env: BTreeMap<_, _> = command
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().to_string(),
                        v.to_string_lossy().to_string(),
                    )
                })
            })
            .collect();
        assert_eq!(env["ANTHROPIC_API_KEY"], "SECRET-API");
        assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "1");
        assert_eq!(env["AZURE_CLIENT_SECRET"], "SECRET-AZURE");
        assert!(!env.contains_key("NODE_OPTIONS"));
        assert!(!format!("{adapter:?}").contains("SECRET"));
    }

    #[test]
    fn narrowing_can_deny_execution_route() {
        let mut config = config();
        config.policy.allowed_routes = Some(Default::default());
        assert!(
            route_identity(&config, &RouteId::new("claude").unwrap())
                .unwrap_err()
                .to_string()
                .contains("denied")
        );
    }

    #[test]
    fn mcp_bridge_authenticates_and_only_dispatches_advertised_tools() {
        let mut bridge = Bridge::start().unwrap();
        let address = bridge.address.clone();
        let secret = bridge.secret.clone();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let record = format!(
                "{}\n",
                json!({"secret":secret,"request":{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"not-advertised","arguments":{}}}})
            );
            stream.write_all(&record.as_bytes()[..1]).unwrap();
            // TCP may split even one write. Deliberately leave a partial frame
            // across several listener polls to exercise the real stream mode.
            std::thread::sleep(Duration::from_millis(100));
            stream.write_all(&record.as_bytes()[1..]).unwrap();
            read_record(&mut BufReader::new(stream)).unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !client.is_finished() && Instant::now() < deadline {
            bridge
                .serve(&json!([]), &mut |_| {
                    panic!("unadvertised effect dispatched")
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(client.join().unwrap()["result"]["isError"], true);
        let config = json!({"env":{BRIDGE_ADDRESS:bridge.address,BRIDGE_SECRET:bridge.secret}});
        assert!(config["env"].get(BRIDGE_ADDRESS).is_some());
    }

    #[test]
    fn duplicate_mcp_request_id_never_reexecutes_an_effect() {
        let (send, calls) = mpsc::sync_channel(2);
        let mut bridge = Bridge {
            address: String::new(),
            secret: String::new(),
            calls,
            stop: Arc::new(AtomicBool::new(false)),
            seen_tools: Default::default(),
        };
        let mut executions = 0;
        for expected_duplicate in [false, true] {
            let (reply, response) = mpsc::sync_channel(1);
            send.send(BridgeCall {
                value: json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"shell","arguments":{}}}), reply,
            }).unwrap();
            bridge
                .serve(&json!([{"name":"shell"}]), &mut |_| {
                    executions += 1;
                    Ok(json!({"content":[]}))
                })
                .unwrap();
            let response = response.recv().unwrap();
            assert_eq!(response.get("error").is_some(), expected_duplicate);
        }
        assert_eq!(executions, 1);
    }

    #[cfg(unix)]
    fn fake_cli(body: &str) -> (tempfile::TempDir, ClaudeCode) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("official CLI with spaces");
        std::fs::write(&program, format!(r#"#!/bin/sh
if [ "$1" = auth ]; then
  echo '{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"}}'
  exit 0
fi
session=''
restricted=no
strict=no
while [ "$#" -gt 0 ]; do
  case "$1" in
    --resume|--session-id) shift; session="$1" ;;
    --restricted) restricted=yes ;;
    --strict-mcp-config) strict=yes ;;
    --tools|--setting-sources) shift; [ -z "$1" ] || exit 51 ;;
  esac
  shift
done
[ "$restricted" = yes ] && [ "$strict" = yes ] || exit 52
[ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ] || exit 53
/bin/cat >/dev/null
printf '{{"type":"system","subtype":"init","session_id":"%s","model":"sonnet","tools":[],"mcp_servers":[{{"name":"zirv","status":"connected"}}]}}\n' "$session"
{body}
"#)).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = ClaudeCode {
            program,
            config_directory: tmp.path().join(".claude"),
            auth: RefCell::default(),
            policy: PolicyRoots::default(),
            directory: tmp.path().to_path_buf(),
            environment: BTreeMap::new(),
            version: "2.1.248".into(),
        };
        (tmp, adapter)
    }

    #[cfg(unix)]
    fn request<'a>(cancel: &'a CancellationFlag) -> ExecutionRequest<'a> {
        ExecutionRequest {
            session: "11111111-1111-4111-8111-111111111111",
            resume: false,
            prompt: "task with shell syntax $(touch SHOULD_NOT_EXIST)",
            system: "instructions",
            model: "sonnet",
            tools: json!([]),
            max_turns: 2,
            timeout: Duration::from_secs(3),
            idle_timeout: Duration::from_secs(2),
            cancel,
        }
    }

    #[cfg(unix)]
    #[test]
    fn fake_process_streams_and_requires_explicit_session_terminal() {
        let (_tmp, adapter) = fake_cli(
            r#"
printf '{"type":"stream_event","session_id":"%s","event":{"delta":{"type":"text_delta","text":"hello"}}}\n' "$session"
printf '{"type":"result","session_id":"%s","subtype":"success","is_error":false,"result":"hello"}\n' "$session"
"#,
        );
        let cancel = CancellationFlag::default();
        let mut observations = Vec::new();
        let result = adapter
            .run(&request(&cancel), &mut |event| {
                observations.push(event);
                Ok(Value::Null)
            })
            .unwrap();
        assert_eq!(result.text, "hello");
        assert!(
            matches!(&observations[..],[ExecutionEvent::Initialized { .. },ExecutionEvent::Text(text)] if text=="hello")
        );
        assert!(!adapter.directory.join("SHOULD_NOT_EXIST").exists());
        assert!(
            !adapter
                .directory
                .join(request(&cancel).session)
                .join("mcp.json")
                .exists()
        );
        let mut followup = request(&cancel);
        followup.resume = true;
        adapter.run(&followup, &mut |_| Ok(Value::Null)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fake_process_exit_crash_stderr_and_timeout_do_not_claim_success() {
        for (body, expected) in [
            ("exit 0", "without terminal result"),
            ("exit 3", "unsuccessfully"),
            ("printf 'malformed\\n'", "malformed NDJSON"),
            ("/bin/sleep 3", "timeout"),
            (
                "i=0; while [ $i -lt 5000 ]; do echo SECRET >&2; i=$((i+1)); done; exit 0",
                "without terminal result",
            ),
        ] {
            let (_tmp, adapter) = fake_cli(body);
            let cancel = CancellationFlag::default();
            let mut request = request(&cancel);
            request.timeout = Duration::from_millis(250);
            let error = adapter
                .run(&request, &mut |_| Ok(Value::Null))
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("SECRET"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn newly_present_managed_policy_blocks_the_next_turn_before_execution() {
        let (_tmp, mut adapter) = fake_cli("exit 93");
        let policy = adapter.config_directory.join("managed");
        std::fs::create_dir_all(&policy).unwrap();
        adapter.policy.managed.push(policy.clone());
        check_settings(&adapter.config_directory, &adapter.policy).unwrap();
        std::fs::write(policy.join("managed-settings.json"), "{}").unwrap();
        let cancel = CancellationFlag::default();
        let error = adapter
            .run(&request(&cancel), &mut |_| {
                panic!("model or effect launched")
            })
            .unwrap_err();
        assert!(error.to_string().contains("managed Claude startup policy"));
        assert!(!adapter.directory.join(request(&cancel).session).exists());
    }

    #[cfg(unix)]
    #[test]
    fn fake_process_cancellation_terminates_a_delayed_child() {
        let (_tmp, adapter) = fake_cli("/bin/sleep 30");
        let cancel = Arc::new(CancellationFlag::default());
        let trigger = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger.cancel();
        });
        let started = Instant::now();
        let error = adapter
            .run(&request(&cancel), &mut |_| Ok(Value::Null))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cancelled"),
            "unexpected cancellation error: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
