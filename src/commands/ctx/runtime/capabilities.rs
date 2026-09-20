//! Configured non-shell capabilities for a native session (issue #483,
//! roadmap N14): web search/fetch, browser automation and inspection,
//! language-diagnostic status, and the auto-discovery that turns all of them
//! into one honest capability report.
//!
//! Three rules shape this module.
//!
//! **These are CONFIGURED capabilities, never model-provided.** A raw model
//! API call gives zirv no search index, no browser and no MCP server. Each
//! backend here exists only when an operator configured one, and a call
//! against an absent backend returns a typed [`CapabilityUnavailable`] naming
//! the missing binary, credential or config key. There is no path through
//! this module that returns an empty success, which is what makes
//! "fabricated capability success is impossible" a structural property rather
//! than a promise.
//!
//! **Every result is traceable.** A web result carries the source URL it came
//! from and the endpoint that produced it; a browser output carries the
//! on-disk evidence path it was written to. A row that cannot name its source
//! is dropped rather than reported.
//!
//! **One egress chokepoint.** Every outbound request this module can make
//! passes [`EgressGuard::inspect`] first. Issue #466's on-device secret/PII
//! obfuscation is expected to replace the default pass-through implementation
//! here rather than build a second interception subsystem beside it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use super::super::config::{
    BrowserCapabilityConfig, CapabilitiesConfig, CtxConfig, EnvLookup, WebCapabilityConfig,
};
use super::super::pace::redact_for_log;
use crate::commands::workflow::capability::{IntegrationId, IntegrationState, IntegrationStatus};

const MAX_WEB_RESULTS: usize = 20;
const MAX_SNIPPET_BYTES: usize = 400;
const MAX_DOM_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCREENSHOT_BYTES: u64 = 16 * 1024 * 1024;
const BROWSER_POLL: Duration = Duration::from_millis(50);

/// Why a configured capability could not run. Always names the concrete thing
/// that is missing, because a workflow admission check quotes it verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityUnavailable {
    pub integration: IntegrationId,
    pub diagnosis: String,
}

impl CapabilityUnavailable {
    fn new(integration: IntegrationId, diagnosis: impl Into<String>) -> Self {
        Self {
            integration,
            diagnosis: diagnosis.into(),
        }
    }
}

impl std::fmt::Display for CapabilityUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is unavailable: {}", self.integration, self.diagnosis)
    }
}

impl std::error::Error for CapabilityUnavailable {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityError {
    Unavailable(CapabilityUnavailable),
    Denied(String),
    Backend(String),
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "{why}"),
            Self::Denied(why) => write!(f, "refused: {why}"),
            Self::Backend(why) => write!(f, "backend failed: {why}"),
        }
    }
}

impl std::error::Error for CapabilityError {}

// --------------------------------------------------------------------------
// egress seam (issue #466)
// --------------------------------------------------------------------------

/// One outbound request, described before it leaves the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressRequest {
    pub integration: IntegrationId,
    pub url: String,
    /// Caller-supplied text that would travel with the request (a search
    /// query, a request body). #466's obfuscator rewrites this.
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressDecision {
    /// Send as-is, or with `payload` replaced by the returned text.
    Allow {
        payload: String,
    },
    Refuse {
        reason: String,
    },
}

/// The single interception point for everything this module sends. Issue
/// #466 (on-device secret and PII obfuscation with lossless placeholders)
/// implements this trait; nothing else in the native runtime needs its own
/// outbound hook.
pub trait EgressGuard: std::fmt::Debug + Send + Sync {
    fn inspect(&self, request: &EgressRequest) -> EgressDecision;
}

/// The default until #466 lands: no rewriting, no refusal, no second
/// obfuscation subsystem invented here.
#[derive(Debug, Default)]
pub struct PassThroughEgress;

impl EgressGuard for PassThroughEgress {
    fn inspect(&self, request: &EgressRequest) -> EgressDecision {
        EgressDecision::Allow {
            payload: request.payload.clone(),
        }
    }
}

// --------------------------------------------------------------------------
// web
// --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpGetReply {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

/// The one outbound call the web backend makes. A trait so search parsing,
/// host enforcement, provenance and bounds are all exercised without network.
pub trait HttpGetter: std::fmt::Debug + Send + Sync {
    fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<HttpGetReply, String>;
}

#[derive(Debug, Default)]
pub struct UreqGetter;

impl HttpGetter for UreqGetter {
    fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<HttpGetReply, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .build()
            .into();
        let mut request = agent.get(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let mut response = request.call().map_err(|error| format!("{error}"))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .body_mut()
            .with_config()
            .limit(max_bytes as u64)
            .read_to_string()
            .map_err(|error| format!("could not read body: {error}"))?;
        Ok(HttpGetReply {
            status,
            content_type,
            body,
        })
    }
}

/// One search hit. `url` is not optional: a row that cannot name where it
/// came from is dropped before it becomes a result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug)]
pub struct WebBackend {
    config: WebCapabilityConfig,
    credential: Option<String>,
    getter: Arc<dyn HttpGetter>,
    egress: Arc<dyn EgressGuard>,
}

impl WebBackend {
    pub fn new(
        config: WebCapabilityConfig,
        credential: Option<String>,
        getter: Arc<dyn HttpGetter>,
        egress: Arc<dyn EgressGuard>,
    ) -> Self {
        Self {
            config,
            credential,
            getter,
            egress,
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.config.timeout_ms_or_default())
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![(
            "user-agent".to_string(),
            format!("zirv/{}", env!("CARGO_PKG_VERSION")),
        )];
        if let Some(credential) = &self.credential {
            headers.push(("authorization".into(), format!("Bearer {credential}")));
        }
        headers
    }

    /// Enforces the operator's host allowlist. An empty allowlist is a closed
    /// door: nothing is reachable until a host is named.
    fn admit_host(&self, url: &str) -> Result<String, CapabilityError> {
        let host = host_of(url).ok_or_else(|| {
            CapabilityError::Denied(format!("{url:?} is not an absolute http(s) URL"))
        })?;
        if self
            .config
            .allow_hosts
            .iter()
            .any(|allowed| host_matches(&host, allowed))
        {
            Ok(host)
        } else {
            Err(CapabilityError::Denied(format!(
                "host {host:?} is not in capabilities.web.allow_hosts"
            )))
        }
    }

    fn send(
        &self,
        integration: IntegrationId,
        url: &str,
        payload: &str,
        max_bytes: usize,
    ) -> Result<HttpGetReply, CapabilityError> {
        let decision = self.egress.inspect(&EgressRequest {
            integration,
            url: url.to_string(),
            payload: payload.to_string(),
        });
        match decision {
            EgressDecision::Allow { .. } => {}
            EgressDecision::Refuse { reason } => return Err(CapabilityError::Denied(reason)),
        }
        self.getter
            .get(url, &self.headers(), max_bytes, self.timeout())
            .map_err(CapabilityError::Backend)
    }

    pub fn search(&self, query: &str) -> Result<Value, CapabilityError> {
        let Some(template) = self.config.search_endpoint.as_deref() else {
            return Err(CapabilityError::Unavailable(CapabilityUnavailable::new(
                IntegrationId::WebSearch,
                "no capabilities.web.search_endpoint is configured; a raw model API provides no \
                 search of its own",
            )));
        };
        let url = template.replace("{query}", &url_encode(query));
        self.admit_host(&url)?;
        let reply = self.send(
            IntegrationId::WebSearch,
            &url,
            query,
            self.config.max_fetch_bytes_or_default(),
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(CapabilityError::Backend(format!(
                "search endpoint answered HTTP {}",
                reply.status
            )));
        }
        let parsed: Value = serde_json::from_str(&reply.body).map_err(|error| {
            CapabilityError::Backend(format!("search endpoint did not answer JSON: {error}"))
        })?;
        let results = parse_web_results(&parsed);
        Ok(json!({
            "query": query,
            "endpoint_host": host_of(&url),
            "result_count": results.len(),
            "results": results,
        }))
    }

    pub fn fetch(&self, url: &str) -> Result<Value, CapabilityError> {
        if !self.config.fetch_enabled {
            return Err(CapabilityError::Unavailable(CapabilityUnavailable::new(
                IntegrationId::WebFetch,
                "capabilities.web.fetch_enabled is false",
            )));
        }
        let host = self.admit_host(url)?;
        let limit = self.config.max_fetch_bytes_or_default();
        let reply = self.send(IntegrationId::WebFetch, url, "", limit)?;
        Ok(json!({
            "url": url,
            "host": host,
            "status": reply.status,
            "content_type": reply.content_type,
            "bytes": reply.body.len(),
            "truncated": reply.body.len() >= limit,
            "body": reply.body,
        }))
    }
}

/// Pulls result rows out of whatever shape a configured endpoint answers.
/// Deliberately permissive about the container and strict about the row: a
/// row without a usable URL is not a result.
fn parse_web_results(value: &Value) -> Vec<WebResult> {
    const CONTAINERS: &[&str] = &["results", "data", "items", "organic_results", "hits"];
    let array = CONTAINERS
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .or_else(|| value.pointer("/webPages/value").and_then(Value::as_array))
        .or_else(|| value.as_array());
    let Some(array) = array else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|row| {
            let url = ["url", "link", "href"]
                .iter()
                .find_map(|key| row.get(*key).and_then(Value::as_str))?;
            // A row whose URL has no parsable host names no source, and an
            // unsourced result is not a result.
            host_of(url)?;
            let title = ["title", "name", "heading"]
                .iter()
                .find_map(|key| row.get(*key).and_then(Value::as_str))
                .unwrap_or_default();
            let snippet = ["snippet", "description", "content", "text"]
                .iter()
                .find_map(|key| row.get(*key).and_then(Value::as_str))
                .unwrap_or_default();
            Some(WebResult {
                title: bounded(title, MAX_SNIPPET_BYTES),
                url: url.to_string(),
                snippet: bounded(snippet, MAX_SNIPPET_BYTES),
            })
        })
        .take(MAX_WEB_RESULTS)
        .collect()
}

pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo (`user:pass@host`) sits before the LAST `@`: an attacker who
    // wants a hostile host to be trusted stuffs a real-looking name into the
    // userinfo slot (`https://allowed.example@evil.example/`), so taking
    // anything but the last segment would be exactly the bug that enables it.
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        // A bracketed IPv6 literal (`[::1]:8080`): the address itself is full
        // of colons, so the closing bracket is the only reliable boundary --
        // splitting on `:` first (as a bare host:port would) mistakes the
        // first colon of the address for a port separator. No closing
        // bracket is malformed input, not a host: fail closed.
        let (host, _after) = bracketed.split_once(']')?;
        host
    } else {
        authority.split(':').next()?
    };
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Exact host, or a single leading-dot suffix (`.example.com`). No wildcards
/// beyond that: an allowlist a model could talk its way past is not one.
fn host_matches(host: &str, allowed: &str) -> bool {
    let allowed = allowed.trim().to_ascii_lowercase();
    if let Some(suffix) = allowed.strip_prefix('.') {
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }
    host == allowed
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn bounded(text: &str, limit: usize) -> String {
    let mut single_line = redact_for_log(text).replace(['\n', '\r'], " ");
    if single_line.len() > limit {
        single_line.truncate(
            (0..=limit)
                .rev()
                .find(|index| single_line.is_char_boundary(*index))
                .unwrap_or(0),
        );
        single_line.push('\u{2026}');
    }
    single_line
}

// --------------------------------------------------------------------------
// browser
// --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserAction {
    /// Write a PNG of `url` to `output`.
    Screenshot {
        width: u32,
        height: u32,
        output: PathBuf,
    },
    /// Return the rendered DOM of `url`.
    Dom,
}

/// Drives a headless Chromium-family browser. A trait so the argv hardening,
/// evidence-path contract and failure classification are tested without a
/// browser installed.
pub trait BrowserRunner: std::fmt::Debug + Send + Sync {
    fn run(&self, url: &str, action: &BrowserAction, timeout: Duration) -> Result<String, String>;
    fn binary(&self) -> String;
}

/// The production runner: the same headless Chromium invocation
/// `frontend render` already uses, so a machine that can capture a frontend
/// render can inspect a page natively too.
#[derive(Debug)]
pub struct ChromiumRunner {
    binary: String,
}

impl ChromiumRunner {
    pub fn new(binary: String) -> Self {
        Self { binary }
    }
}

impl BrowserRunner for ChromiumRunner {
    fn run(&self, url: &str, action: &BrowserAction, timeout: Duration) -> Result<String, String> {
        let mut command = Command::new(&self.binary);
        command
            .arg("--headless=new")
            .arg("--disable-background-networking")
            .arg("--disable-component-update")
            .arg("--disable-default-apps")
            .arg("--disable-extensions")
            .arg("--disable-sync")
            .arg("--metrics-recording-only")
            .arg("--no-first-run")
            .arg("--virtual-time-budget=2000")
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        match action {
            BrowserAction::Screenshot {
                width,
                height,
                output,
            } => {
                command
                    .arg(format!("--window-size={width},{height}"))
                    .arg(format!("--screenshot={}", output.display()))
                    .stdout(Stdio::null());
            }
            BrowserAction::Dom => {
                command.arg("--dump-dom").stdout(Stdio::piped());
            }
        }
        command.arg(url);
        crate::commands::workflow::isolate_process_tree(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", self.binary))?;
        if matches!(action, BrowserAction::Dom) {
            // stdout is a pipe: read it to completion, which is also how the
            // child is reaped. `output` would re-spawn, so wait_with_output
            // on the already-spawned child is the correct shape here.
            let output = child
                .wait_with_output()
                .map_err(|error| format!("browser failed: {error}"))?;
            if !output.status.success() {
                return Err(format!("browser exited with {}", output.status));
            }
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.truncate(
                (0..=text.len().min(MAX_DOM_BYTES))
                    .rev()
                    .find(|index| text.is_char_boundary(*index))
                    .unwrap_or(0),
            );
            return Ok(text);
        }
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(String::new()),
                Ok(Some(status)) => return Err(format!("browser exited with {status}")),
                Ok(None) => {}
                Err(error) => return Err(format!("browser failed: {error}")),
            }
            if started.elapsed() > timeout {
                let _ = crate::commands::workflow::terminate_process_tree(&mut child);
                return Err(format!("browser timed out after {timeout:?}"));
            }
            std::thread::sleep(BROWSER_POLL);
        }
    }

    fn binary(&self) -> String {
        self.binary.clone()
    }
}

#[derive(Debug)]
pub struct BrowserBackend {
    runner: Arc<dyn BrowserRunner>,
    timeout: Duration,
    egress: Arc<dyn EgressGuard>,
}

impl BrowserBackend {
    pub fn new(
        runner: Arc<dyn BrowserRunner>,
        config: &BrowserCapabilityConfig,
        egress: Arc<dyn EgressGuard>,
    ) -> Self {
        Self {
            runner,
            timeout: Duration::from_millis(config.timeout_ms_or_default()),
            egress,
        }
    }

    fn admit(&self, url: &str) -> Result<(), CapabilityError> {
        match self.egress.inspect(&EgressRequest {
            integration: IntegrationId::Browser,
            url: url.to_string(),
            payload: String::new(),
        }) {
            EgressDecision::Allow { .. } => Ok(()),
            EgressDecision::Refuse { reason } => Err(CapabilityError::Denied(reason)),
        }
    }

    /// Captures `url` to `output` and returns a result that links to the file
    /// actually on disk. A capture that produced no readable file is an
    /// error, never a success with a path nobody checked.
    pub fn capture(
        &self,
        url: &str,
        output: &Path,
        width: u32,
        height: u32,
    ) -> Result<Value, CapabilityError> {
        self.admit(url)?;
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| CapabilityError::Backend(error.to_string()))?;
        }
        self.runner
            .run(
                url,
                &BrowserAction::Screenshot {
                    width,
                    height,
                    output: output.to_path_buf(),
                },
                self.timeout,
            )
            .map_err(CapabilityError::Backend)?;
        let metadata = std::fs::metadata(output).map_err(|error| {
            CapabilityError::Backend(format!(
                "browser reported success but wrote no evidence at {}: {error}",
                output.display()
            ))
        })?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SCREENSHOT_BYTES {
            return Err(CapabilityError::Backend(format!(
                "browser evidence at {} is {} bytes, which is not a usable screenshot",
                output.display(),
                metadata.len()
            )));
        }
        Ok(json!({
            "url": url,
            "browser": self.runner.binary(),
            "evidence_path": output.display().to_string(),
            "bytes": metadata.len(),
            "viewport": {"width": width, "height": height},
        }))
    }

    pub fn inspect(&self, url: &str) -> Result<Value, CapabilityError> {
        self.admit(url)?;
        let dom = self
            .runner
            .run(url, &BrowserAction::Dom, self.timeout)
            .map_err(CapabilityError::Backend)?;
        Ok(json!({
            "url": url,
            "browser": self.runner.binary(),
            "bytes": dom.len(),
            "dom": dom,
        }))
    }
}

// --------------------------------------------------------------------------
// diagnostics
// --------------------------------------------------------------------------

/// What language/diagnostic tooling this repository ACTUALLY has, probed on
/// PATH and in the tree. Never an imaginary IDE feature: a tool that is not
/// installed is reported absent, with the binary name someone has to install.
pub fn diagnostics_report(repo: &Path) -> Value {
    let checker = super::super::diagnostics::checker_for(repo);
    let tools: Vec<Value> = [
        (
            "cargo",
            "Rust build and check",
            repo.join("Cargo.toml").is_file(),
        ),
        (
            "rust-analyzer",
            "Rust language server",
            repo.join("Cargo.toml").is_file(),
        ),
        (
            "tsc",
            "TypeScript type checker",
            repo.join("tsconfig.json").is_file(),
        ),
        (
            "eslint",
            "JavaScript/TypeScript linter",
            repo.join("package.json").is_file(),
        ),
    ]
    .into_iter()
    .map(|(program, purpose, relevant)| {
        let present = super::super::adapters::program_is_present(program);
        json!({
            "program": program,
            "purpose": purpose,
            "relevant_to_repo": relevant,
            "installed": present,
            "diagnosis": if present {
                Value::Null
            } else {
                Value::String(format!("`{program}` is not on PATH"))
            },
        })
    })
    .collect();
    json!({
        "repo": repo.display().to_string(),
        "post_edit_checker": match checker {
            Some(super::super::diagnostics::Checker::Cargo) => Some("cargo"),
            Some(super::super::diagnostics::Checker::Tsc) => Some("tsc"),
            None => None,
        },
        "tools": tools,
    })
}

// --------------------------------------------------------------------------
// discovery
// --------------------------------------------------------------------------

/// Auto-discovers every configured integration within the authorization that
/// already exists: config the operator wrote, binaries on PATH, files in the
/// tree. It contacts nothing -- no MCP server is spawned, no HTTP request is
/// made -- so it is cheap enough to run on every workflow admission check.
///
/// That is exactly why the third state exists. A configured MCP server whose
/// process has not been started this run is `unverified`, not `available`:
/// zirv has no evidence it answers. `zirv ctx capabilities --probe` is the
/// surface that converts an `unverified` row into a verified one by actually
/// connecting.
pub fn discover(cfg: &CtxConfig, repo: &Path) -> Vec<IntegrationStatus> {
    let capabilities = &cfg.capabilities;
    let mut rows = Vec::new();

    let servers: Vec<&str> = capabilities
        .active_servers()
        .map(|server| server.name.as_str())
        .collect();
    rows.push(if !capabilities.enabled {
        IntegrationStatus::unavailable(
            IntegrationId::Mcp,
            "capabilities.enabled is false",
            "set capabilities.enabled in ~/.zirv/ctx.toml or ZIRV_CTX_CAPABILITIES",
        )
    } else if servers.is_empty() {
        IntegrationStatus::unavailable(
            IntegrationId::Mcp,
            "no MCP server is configured or enabled",
            "add a [[capabilities.mcp]] entry with enabled = true",
        )
    } else {
        IntegrationStatus::unverified(
            IntegrationId::Mcp,
            format!(
                "{} configured server(s): {}",
                servers.len(),
                servers.join(", ")
            ),
            "configured but not contacted this run; run `zirv ctx capabilities --probe`",
        )
    });

    rows.push(named_mcp_status(
        IntegrationId::Linear,
        capabilities,
        "linear",
        |name| name.eq_ignore_ascii_case("linear"),
    ));
    rows.push(named_mcp_status(
        IntegrationId::Kibana,
        capabilities,
        "kibana",
        |name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "kibana" | "elastic" | "elasticsearch"
            )
        },
    ));

    rows.push(web_status(
        IntegrationId::WebSearch,
        capabilities.enabled,
        &capabilities.web,
    ));
    rows.push(web_status(
        IntegrationId::WebFetch,
        capabilities.enabled,
        &capabilities.web,
    ));

    rows.push(match browser_binary(&capabilities.browser) {
        _ if !capabilities.enabled => IntegrationStatus::unavailable(
            IntegrationId::Browser,
            "capabilities.enabled is false",
            "set capabilities.enabled in ~/.zirv/ctx.toml or ZIRV_CTX_CAPABILITIES",
        ),
        _ if !capabilities.browser.enabled => IntegrationStatus::unavailable(
            IntegrationId::Browser,
            "capabilities.browser.enabled is false",
            "set capabilities.browser.enabled = true",
        ),
        Some(binary) => IntegrationStatus::available(
            IntegrationId::Browser,
            format!("headless browser `{binary}`"),
        ),
        None => IntegrationStatus::unavailable(
            IntegrationId::Browser,
            "no Chromium-family browser was discovered",
            "install chromium/google-chrome/microsoft-edge, or set \
             capabilities.browser.binary",
        ),
    });

    let diagnostics = diagnostics_report(repo);
    rows.push(match diagnostics["post_edit_checker"].as_str() {
        Some(checker) => IntegrationStatus::available(
            IntegrationId::Diagnostics,
            format!("`{checker}` checks this repository"),
        ),
        None => IntegrationStatus::unavailable(
            IntegrationId::Diagnostics,
            "no language checker matches this repository",
            "no Cargo.toml, and no tsconfig.json with `tsc` on PATH",
        ),
    });

    // Artifact rendering and frontend capture are zirv's own services and
    // need no operator configuration; the frontend one still depends on the
    // same browser binary, so it degrades with it rather than claiming more.
    rows.push(IntegrationStatus::available(
        IntegrationId::ArtifactRender,
        "zirv artifact registry and static renderer",
    ));
    rows.push(match browser_binary(&capabilities.browser) {
        Some(binary) => IntegrationStatus::available(
            IntegrationId::FrontendRender,
            format!("frontend render and capture via `{binary}`"),
        ),
        None => IntegrationStatus::unavailable(
            IntegrationId::FrontendRender,
            "frontend capture needs a headless browser",
            "install chromium/google-chrome/microsoft-edge, or set \
             capabilities.browser.binary",
        ),
    });

    rows
}

/// A specific-backend integration (issue #539: Linear, Kibana) is never
/// inferred from the generic `mcp` row above -- a skill that names one of
/// these needs THAT server, not just some server, configured and enabled.
/// Matching is ASCII-case-insensitive on the server's declared name, per
/// `matches`. Like the generic MCP row, this never reports `available` from
/// configuration alone: an enabled, name-matching entry is `unverified`
/// until `zirv ctx capabilities --probe` actually contacts it.
fn named_mcp_status(
    integration: IntegrationId,
    capabilities: &CapabilitiesConfig,
    label: &str,
    matches: impl Fn(&str) -> bool,
) -> IntegrationStatus {
    if !capabilities.enabled {
        return IntegrationStatus::unavailable(
            integration,
            "capabilities.enabled is false",
            "set capabilities.enabled in ~/.zirv/ctx.toml or ZIRV_CTX_CAPABILITIES",
        );
    }
    match capabilities
        .active_servers()
        .find(|server| matches(&server.name))
    {
        Some(server) => IntegrationStatus::unverified(
            integration,
            format!("MCP server `{}`", server.name),
            "configured but not contacted this run; run `zirv ctx capabilities --probe`",
        ),
        None => IntegrationStatus::unavailable(
            integration,
            format!("no MCP server named `{label}` is configured or enabled"),
            format!("add a [[capabilities.mcp]] entry named `{label}` with enabled = true"),
        ),
    }
}

fn web_status(
    integration: IntegrationId,
    enabled: bool,
    web: &WebCapabilityConfig,
) -> IntegrationStatus {
    if !enabled {
        return IntegrationStatus::unavailable(
            integration,
            "capabilities.enabled is false",
            "set capabilities.enabled in ~/.zirv/ctx.toml or ZIRV_CTX_CAPABILITIES",
        );
    }
    if web.allow_hosts.is_empty() {
        return IntegrationStatus::unavailable(
            integration,
            "capabilities.web.allow_hosts is empty",
            "name the hosts this machine may reach in capabilities.web.allow_hosts",
        );
    }
    match integration {
        IntegrationId::WebSearch => match web.search_endpoint.as_deref() {
            Some(endpoint) => IntegrationStatus::unverified(
                integration,
                format!(
                    "search endpoint host {}",
                    host_of(endpoint).unwrap_or_else(|| "unparsable".into())
                ),
                "configured but not called this run; a raw model API provides no search",
            ),
            None => IntegrationStatus::unavailable(
                integration,
                "no capabilities.web.search_endpoint is configured",
                "set capabilities.web.search_endpoint to a JSON search endpoint",
            ),
        },
        _ if web.fetch_enabled => IntegrationStatus::available(
            integration,
            format!("{} allowed host(s)", web.allow_hosts.len()),
        ),
        _ => IntegrationStatus::unavailable(
            integration,
            "capabilities.web.fetch_enabled is false",
            "set capabilities.web.fetch_enabled = true",
        ),
    }
}

/// The browser this machine would actually use: the operator's explicit
/// choice if it resolves, otherwise the same discovery `frontend render` does.
pub fn browser_binary(config: &BrowserCapabilityConfig) -> Option<String> {
    if let Some(binary) = config.binary.as_deref() {
        let present =
            Path::new(binary).is_file() || super::super::adapters::program_is_present(binary);
        return present.then(|| binary.to_string());
    }
    crate::commands::workflow::frontend_render::discover_browser()
}

/// Resolves the configured search credential through the same store the
/// direct providers use. A reference that does not resolve is `None` with a
/// reason, never a silent empty bearer token.
pub fn resolve_credential(
    reference: Option<&str>,
    env: EnvLookup<'_>,
    now: u64,
) -> Result<Option<String>, String> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let parsed: super::super::provider::credential::CredentialRef = reference.parse()?;
    let store = super::super::provider::credential::OsStore::default();
    super::super::provider::credential::resolve(&parsed, env, &store, now)
        .map(|credential| Some(credential.secret.expose().to_string()))
        .map_err(|error| error.to_string())
}

/// Everything a native session's tools need to reach a configured
/// capability, assembled once per session.
#[derive(Debug, Default)]
pub struct CapabilityServices {
    pub web: Option<WebBackend>,
    pub browser: Option<BrowserBackend>,
    pub integrations: Vec<IntegrationStatus>,
    config: super::super::config::CapabilitiesConfig,
    /// Lazily connected clients. A session that never calls an MCP tool never
    /// spawns a server, so starting one does not depend on a server being up.
    clients: BTreeMap<String, super::mcp::McpClient>,
    /// Test seam: a factory injected here replaces the configured transport
    /// for that server, which is how the registry path is exercised against
    /// the in-process fixture server.
    pub transport_overrides: BTreeMap<String, Arc<dyn super::mcp::TransportFactory>>,
}

impl CapabilityServices {
    /// Builds the backends an operator configured. MCP servers are NOT
    /// connected here: a session connects lazily on first use, so starting a
    /// session never depends on a server being up.
    pub fn from_config(cfg: &CtxConfig, repo: &Path, env: EnvLookup<'_>, now: u64) -> Self {
        let egress: Arc<dyn EgressGuard> = Arc::new(PassThroughEgress);
        let integrations = discover(cfg, repo);
        if !cfg.capabilities.enabled {
            return Self {
                integrations,
                ..Self::default()
            };
        }
        let credential =
            resolve_credential(cfg.capabilities.web.search_credential.as_deref(), env, now)
                .unwrap_or(None);
        let web = Some(WebBackend::new(
            cfg.capabilities.web.clone(),
            credential,
            Arc::new(UreqGetter),
            Arc::clone(&egress),
        ));
        let browser = if cfg.capabilities.browser.enabled {
            browser_binary(&cfg.capabilities.browser).map(|binary| {
                BrowserBackend::new(
                    Arc::new(ChromiumRunner::new(binary)),
                    &cfg.capabilities.browser,
                    Arc::clone(&egress),
                )
            })
        } else {
            None
        };
        Self {
            web,
            browser,
            integrations,
            config: cfg.capabilities.clone(),
            clients: BTreeMap::new(),
            transport_overrides: BTreeMap::new(),
        }
    }

    /// A services bundle for a test or a caller that supplies its own
    /// backends, with discovery already run against `cfg`.
    pub fn for_servers(cfg: &CtxConfig, repo: &Path) -> Self {
        Self {
            integrations: discover(cfg, repo),
            config: cfg.capabilities.clone(),
            ..Self::default()
        }
    }

    pub fn state(&self, integration: IntegrationId) -> IntegrationState {
        self.integrations
            .iter()
            .find(|status| status.integration == integration)
            .map_or(IntegrationState::Unavailable, |status| status.state)
    }

    pub fn server_names(&self) -> Vec<String> {
        self.config
            .active_servers()
            .map(|server| server.name.clone())
            .collect()
    }

    pub fn max_inline_mcp_tools(&self) -> usize {
        self.config.max_inline_mcp_tools_or_default()
    }

    fn factory(
        &self,
        server: &super::super::config::McpServerConfig,
        broker: &super::enforcement::ExecutionBroker,
    ) -> Result<Arc<dyn super::mcp::TransportFactory>, CapabilityError> {
        if let Some(override_factory) = self.transport_overrides.get(&server.name) {
            return Ok(Arc::clone(override_factory));
        }
        Ok(match &server.transport {
            super::super::config::McpTransportConfig::Stdio { command, .. } => {
                if command.trim().is_empty() {
                    return Err(CapabilityError::Unavailable(CapabilityUnavailable::new(
                        IntegrationId::Mcp,
                        format!("server `{}` has an empty stdio command", server.name),
                    )));
                }
                Arc::new(
                    super::mcp::StdioFactory::new(server.clone(), broker)
                        .map_err(|error| CapabilityError::Backend(error.to_string()))?,
                )
            }
            super::super::config::McpTransportConfig::Http { url, credential } => {
                let reference = credential.clone();
                Arc::new(super::mcp::HttpFactory::new_resolving(
                    url.clone(),
                    Arc::new(move || {
                        let env = super::super::config::env_from_process();
                        resolve_credential(
                            reference.as_deref(),
                            &env,
                            super::super::state::now_secs(),
                        )
                        .map_err(super::mcp::McpError::Unavailable)
                    }),
                    Arc::new(super::mcp::UreqPoster),
                ))
            }
        })
    }

    /// The connected client for `name`, connecting on first use. A server
    /// that is not configured, not enabled, or whose credential did not
    /// resolve is a typed `Unavailable`, never a client that looks connected.
    pub fn client(
        &mut self,
        name: &str,
        broker: &super::enforcement::ExecutionBroker,
    ) -> Result<&mut super::mcp::McpClient, CapabilityError> {
        if !self.clients.contains_key(name) {
            let server = self
                .config
                .active_servers()
                .find(|server| server.name == name)
                .cloned()
                .ok_or_else(|| {
                    CapabilityError::Unavailable(CapabilityUnavailable::new(
                        IntegrationId::Mcp,
                        format!("no enabled [[capabilities.mcp]] entry is named `{name}`"),
                    ))
                })?;
            let factory = self.factory(&server, broker)?;
            let client = super::mcp::McpClient::connect(
                &server.name,
                factory,
                super::enforcement::ProcessEffects::from(&server.effects),
                Duration::from_millis(server.request_timeout_ms_or_default()),
            )
            .map_err(|error| CapabilityError::Backend(error.to_string()))?;
            self.clients.insert(name.to_string(), client);
        }
        self.clients
            .get_mut(name)
            .ok_or_else(|| CapabilityError::Backend("MCP client vanished".into()))
    }

    /// The declared effects for one configured server. Trusted operator
    /// config, never a server's own claim about itself.
    pub fn server_effects(&self, name: &str) -> Option<super::enforcement::ProcessEffects> {
        self.config
            .active_servers()
            .find(|server| server.name == name)
            .map(|server| super::enforcement::ProcessEffects::from(&server.effects))
    }

    pub fn shutdown(&mut self) {
        for client in self.clients.values_mut() {
            client.shutdown();
        }
        self.clients.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct ScriptedGetter {
        reply: HttpGetReply,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedGetter {
        fn json(body: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: HttpGetReply {
                    status: 200,
                    content_type: "application/json".into(),
                    body: body.to_string(),
                },
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl HttpGetter for ScriptedGetter {
        fn get(
            &self,
            url: &str,
            _headers: &[(String, String)],
            _max_bytes: usize,
            _timeout: Duration,
        ) -> Result<HttpGetReply, String> {
            self.seen.lock().expect("lock").push(url.to_string());
            Ok(self.reply.clone())
        }
    }

    fn web(config: WebCapabilityConfig, getter: Arc<dyn HttpGetter>) -> WebBackend {
        WebBackend::new(config, None, getter, Arc::new(PassThroughEgress))
    }

    fn configured_web() -> WebCapabilityConfig {
        WebCapabilityConfig {
            search_endpoint: Some("https://search.example/q?s={query}".into()),
            fetch_enabled: true,
            allow_hosts: vec!["search.example".into(), ".docs.example".into()],
            ..WebCapabilityConfig::default()
        }
    }

    #[test]
    fn web_search_without_a_configured_backend_is_unavailable_not_an_empty_success() {
        let backend = web(
            WebCapabilityConfig {
                allow_hosts: vec!["search.example".into()],
                ..WebCapabilityConfig::default()
            },
            ScriptedGetter::json("{}"),
        );
        let error = backend
            .search("rust")
            .expect_err("no endpoint is configured");
        assert!(
            matches!(
                &error,
                CapabilityError::Unavailable(why) if why.integration == IntegrationId::WebSearch
            ),
            "{error:?}"
        );
    }

    #[test]
    fn every_web_result_carries_the_source_url_it_came_from() {
        let getter = ScriptedGetter::json(
            r#"{"results":[
                {"title":"A","url":"https://docs.example/a","snippet":"first"},
                {"title":"No source","snippet":"dropped"},
                {"title":"B","link":"https://docs.example/b"}
            ]}"#,
        );
        let backend = web(configured_web(), Arc::clone(&getter) as Arc<dyn HttpGetter>);
        let value = backend.search("rust traits").expect("search");
        assert_eq!(value["result_count"], 2);
        assert_eq!(value["results"][0]["url"], "https://docs.example/a");
        assert_eq!(value["results"][1]["url"], "https://docs.example/b");
        let seen = getter.seen.lock().expect("lock");
        assert_eq!(seen[0], "https://search.example/q?s=rust+traits");
    }

    #[test]
    fn the_host_allowlist_is_a_closed_door_and_matches_only_exact_or_dotted_suffixes() {
        let backend = web(configured_web(), ScriptedGetter::json("{}"));
        assert!(backend.fetch("https://sub.docs.example/page").is_ok());
        assert!(backend.fetch("https://docs.example/page").is_ok());
        let denied = backend
            .fetch("https://evil.example/page")
            .expect_err("outside the allowlist");
        assert!(matches!(denied, CapabilityError::Denied(_)), "{denied:?}");
        let not_a_url = backend
            .fetch("file:///etc/passwd")
            .expect_err("not an http(s) URL");
        assert!(
            matches!(not_a_url, CapabilityError::Denied(_)),
            "{not_a_url:?}"
        );

        let closed = web(
            WebCapabilityConfig {
                fetch_enabled: true,
                ..WebCapabilityConfig::default()
            },
            ScriptedGetter::json("{}"),
        );
        assert!(
            closed.fetch("https://docs.example/page").is_err(),
            "an empty allowlist must reach nothing"
        );
    }

    #[test]
    fn a_bracketed_ipv6_host_matches_its_allowlist_entry_and_userinfo_cannot_launder_a_host() {
        let backend = web(
            WebCapabilityConfig {
                fetch_enabled: true,
                allow_hosts: vec!["::1".into(), "allowed.example".into()],
                ..WebCapabilityConfig::default()
            },
            ScriptedGetter::json("{}"),
        );
        assert!(
            backend.fetch("https://[::1]:8080/status").is_ok(),
            "a bracketed IPv6 authority must resolve to host `::1`, not `[`"
        );

        // Userinfo sits before the LAST `@`; the real host here is
        // evil.example, with the trusted-looking name stuffed into the
        // userinfo slot. Letting that host through would be exactly the kind
        // of allowlist bypass a fixed hostname check exists to prevent.
        let denied = backend
            .fetch("https://allowed.example@evil.example/page")
            .expect_err("the real host is evil.example, not allowed.example");
        assert!(matches!(denied, CapabilityError::Denied(_)), "{denied:?}");
    }

    #[test]
    fn the_egress_seam_can_refuse_an_outbound_request_without_a_second_subsystem() {
        #[derive(Debug)]
        struct RefuseAll;
        impl EgressGuard for RefuseAll {
            fn inspect(&self, request: &EgressRequest) -> EgressDecision {
                EgressDecision::Refuse {
                    reason: format!("interceptor refused {}", request.url),
                }
            }
        }
        let backend = WebBackend::new(
            configured_web(),
            None,
            ScriptedGetter::json("{}"),
            Arc::new(RefuseAll),
        );
        let error = backend
            .fetch("https://docs.example/page")
            .expect_err("the interceptor refuses");
        assert!(matches!(error, CapabilityError::Denied(_)), "{error:?}");
    }

    #[derive(Debug)]
    struct FakeBrowser {
        write: bool,
        dom: String,
    }

    impl BrowserRunner for FakeBrowser {
        fn run(
            &self,
            _url: &str,
            action: &BrowserAction,
            _timeout: Duration,
        ) -> Result<String, String> {
            match action {
                BrowserAction::Screenshot { output, .. } => {
                    if self.write {
                        std::fs::write(output, b"\x89PNG evidence").map_err(|e| e.to_string())?;
                    }
                    Ok(String::new())
                }
                BrowserAction::Dom => Ok(self.dom.clone()),
            }
        }

        fn binary(&self) -> String {
            "fake-chromium".into()
        }
    }

    fn browser(write: bool) -> BrowserBackend {
        BrowserBackend::new(
            Arc::new(FakeBrowser {
                write,
                dom: "<html><body>hi</body></html>".into(),
            }),
            &BrowserCapabilityConfig::default(),
            Arc::new(PassThroughEgress),
        )
    }

    #[test]
    fn a_browser_capture_links_to_evidence_that_actually_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shots/home.png");
        let value = browser(true)
            .capture("https://docs.example/", &path, 1280, 800)
            .expect("capture");
        assert_eq!(value["evidence_path"], path.display().to_string());
        assert!(path.is_file(), "the evidence file must exist");
        assert_eq!(value["bytes"], 13);
    }

    #[test]
    fn a_browser_that_reports_success_without_writing_evidence_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.png");
        let error = browser(false)
            .capture("https://docs.example/", &path, 800, 600)
            .expect_err("no evidence, no success");
        assert!(matches!(error, CapabilityError::Backend(_)), "{error:?}");
    }

    #[test]
    fn a_browser_inspection_returns_the_rendered_dom_with_its_url() {
        let value = browser(true)
            .inspect("https://docs.example/page")
            .expect("inspect");
        assert_eq!(value["url"], "https://docs.example/page");
        assert!(
            value["dom"]
                .as_str()
                .expect("dom")
                .contains("<body>hi</body>")
        );
    }

    #[test]
    fn discovery_reports_an_unconfigured_machine_as_unavailable_with_a_diagnosis() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let rows = discover(&cfg, repo.path());
        let mcp = rows
            .iter()
            .find(|row| row.integration == IntegrationId::Mcp)
            .expect("an MCP row");
        assert_eq!(mcp.state, IntegrationState::Unavailable);
        assert!(
            mcp.diagnosis
                .as_deref()
                .is_some_and(|text| text.contains("capabilities.enabled")),
            "{mcp:?}"
        );
    }

    #[test]
    fn a_configured_but_uncontacted_mcp_server_is_unverified_rather_than_available() {
        use crate::commands::ctx::config::{
            CapabilitiesConfig, McpServerConfig, McpTransportConfig,
        };

        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig {
            capabilities: CapabilitiesConfig {
                enabled: true,
                mcp: vec![McpServerConfig {
                    name: "docs".into(),
                    enabled: true,
                    transport: McpTransportConfig::Stdio {
                        command: "mcp-docs".into(),
                        args: Vec::new(),
                        cwd: None,
                        environment: BTreeMap::new(),
                    },
                    ..McpServerConfig::default()
                }],
                ..CapabilitiesConfig::default()
            },
            ..CtxConfig::default()
        };
        let rows = discover(&cfg, repo.path());
        let mcp = rows
            .iter()
            .find(|row| row.integration == IntegrationId::Mcp)
            .expect("an MCP row");
        assert_eq!(mcp.state, IntegrationState::Unverified);
        assert!(mcp.detail.contains("docs"), "{mcp:?}");
    }

    #[test]
    fn the_diagnostics_report_names_real_tools_and_never_an_imaginary_ide() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("Cargo.toml"), "[package]\n").expect("write");
        let value = diagnostics_report(repo.path());
        assert_eq!(value["post_edit_checker"], "cargo");
        let tools = value["tools"].as_array().expect("tools");
        assert!(tools.iter().all(|tool| tool["installed"].is_boolean()));
        assert!(
            tools
                .iter()
                .any(|tool| tool["program"] == "rust-analyzer" && tool["relevant_to_repo"] == true)
        );
    }

    /// Issue #539: Linear and Kibana are never inferred from the generic
    /// `mcp` row -- a skill naming one of them needs THAT server, not just
    /// some server, and never gets `Available` from configuration alone.
    #[test]
    fn linear_and_kibana_need_a_specifically_named_and_enabled_mcp_server() {
        use crate::commands::ctx::config::{
            CapabilitiesConfig, McpServerConfig, McpTransportConfig,
        };

        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let rows = discover(&cfg, repo.path());
        let linear = rows
            .iter()
            .find(|row| row.integration == IntegrationId::Linear)
            .expect("a linear row");
        assert_eq!(linear.state, IntegrationState::Unavailable);
        assert!(
            linear
                .diagnosis
                .as_deref()
                .is_some_and(|text| text.contains("capabilities.enabled")),
            "{linear:?}"
        );

        let cfg = CtxConfig {
            capabilities: CapabilitiesConfig {
                enabled: true,
                mcp: vec![
                    McpServerConfig {
                        name: "docs".into(),
                        enabled: true,
                        transport: McpTransportConfig::Stdio {
                            command: "mcp-docs".into(),
                            args: Vec::new(),
                            cwd: None,
                            environment: BTreeMap::new(),
                        },
                        ..McpServerConfig::default()
                    },
                    McpServerConfig {
                        name: "Elastic".into(),
                        enabled: true,
                        transport: McpTransportConfig::Stdio {
                            command: "mcp-elastic".into(),
                            args: Vec::new(),
                            cwd: None,
                            environment: BTreeMap::new(),
                        },
                        ..McpServerConfig::default()
                    },
                ],
                ..CapabilitiesConfig::default()
            },
            ..CtxConfig::default()
        };
        let rows = discover(&cfg, repo.path());
        let linear = rows
            .iter()
            .find(|row| row.integration == IntegrationId::Linear)
            .expect("a linear row");
        assert_eq!(
            linear.state,
            IntegrationState::Unavailable,
            "an unrelated configured server never satisfies a named integration"
        );
        assert!(
            linear
                .diagnosis
                .as_deref()
                .is_some_and(|text| text.contains("linear")),
            "{linear:?}"
        );
        let kibana = rows
            .iter()
            .find(|row| row.integration == IntegrationId::Kibana)
            .expect("a kibana row");
        assert_eq!(
            kibana.state,
            IntegrationState::Unverified,
            "case-insensitive `elastic` matches the kibana integration"
        );
        assert!(kibana.detail.contains("Elastic"), "{kibana:?}");
    }
}
