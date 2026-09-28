//! Typed arguments for the configured-capability tools (issue #483, roadmap
//! N14): MCP, web, browser, diagnostics, artifacts and frontend.
//!
//! These parse exactly like N05's file and process tools -- a closed schema, a
//! complete typed request, no filesystem or network reach until the N04 broker
//! has admitted an action. They live beside `files.rs` and `process.rs`
//! because they are the same kind of thing: the only difference is which
//! service the admitted action finally reaches.
//!
//! Two shapes are deliberate.
//!
//! Evidence paths are zirv-chosen. A browser capture names a *label*, not a
//! path: the tool slugifies it and writes under a state-dir evidence root, the
//! same way the output store hands back opaque ids instead of accepting a
//! caller path. Provider output therefore cannot steer where a screenshot
//! lands.
//!
//! MCP tool names are namespaced `mcp__<server>__<tool>` when a small
//! catalogue is inlined into the registry, so a server can never shadow a
//! built-in tool name no matter what it calls its tools.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{CapturePayload, ToolError, ToolErrorCode, mcp_error, persist_capture};
use crate::commands::ctx::output::CompactionScope;
use crate::commands::ctx::runtime::enforcement::{ExecutionAction, ProcessEffects};
use serde_json::{Value, json};

pub(super) const MCP_PREFIX: &str = "mcp__";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WebSearchArgs {
    pub query: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WebFetchArgs {
    pub url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserCaptureArgs {
    pub url: String,
    /// A human label for the capture. Slugified into a zirv-owned evidence
    /// path; never used as a path itself.
    pub label: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    800
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserInspectArgs {
    pub url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpListArgs {
    /// Limits the index to one configured server. Absent means every one.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpDescribeArgs {
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpCallArgs {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactRegisterArgs {
    pub path: PathBuf,
    #[serde(default)]
    pub kind: Option<ArtifactKindArg>,
    #[serde(default)]
    pub workflow_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ArtifactKindArg {
    Image,
    Svg,
    Html,
    Diagram,
    Document,
    Video,
    Other,
}

impl ArtifactKindArg {
    pub(super) fn kind(self) -> crate::commands::workflow::artifact::ArtifactKind {
        use crate::commands::workflow::artifact::ArtifactKind;
        match self {
            Self::Image => ArtifactKind::Image,
            Self::Svg => ArtifactKind::Svg,
            Self::Html => ArtifactKind::Html,
            Self::Diagram => ArtifactKind::Diagram,
            Self::Document => ArtifactKind::Document,
            Self::Video => ArtifactKind::Video,
            Self::Other => ArtifactKind::Other,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactPresentArgs {
    pub id: String,
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrontendReviewArgs {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Turns a caller label into one safe path segment. Everything outside
/// `[a-z0-9-]` becomes a dash, so no separator, drive letter, `..` or NUL can
/// survive into a path.
pub(super) fn evidence_slug(label: &str) -> String {
    let mut slug = String::with_capacity(label.len());
    for character in label.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(64);
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "capture".to_string()
    } else {
        trimmed
    }
}

/// The zirv-owned root every browser evidence file lands under. Inside the
/// state directory, per repository slug, so a capture is findable from the
/// CLI and from a headless result without the model choosing where it went.
pub(super) fn evidence_root(state: &crate::commands::ctx::state::StateDir, repo: &Path) -> PathBuf {
    state
        .root()
        .join("native-evidence")
        .join(crate::commands::ctx::state::repo_slug(repo))
}

/// Parses an absolute http(s) URL into a broker network target, so the host
/// crosses the same allowlist an operator configured for every other effect.
pub(super) fn network_target(
    url: &str,
) -> Result<crate::commands::ctx::runtime::enforcement::NetworkTarget, ToolError> {
    use crate::commands::ctx::runtime::capabilities::host_of;

    let scheme = if url.starts_with("https://") {
        "https"
    } else if url.starts_with("http://") {
        "http"
    } else {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{url:?} must be an absolute http:// or https:// URL"),
        ));
    };
    let host = host_of(url).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{url:?} has no host"),
        )
    })?;
    crate::commands::ctx::runtime::enforcement::NetworkTarget::new(scheme, &host, None)
        .map_err(|error| ToolError::new(ToolErrorCode::InvalidArguments, error.to_string()))
}

impl super::NativeToolClient {
    /// Substitutes the operator's own declared effects for an MCP action
    /// before the broker prices it. Provider output supplies the server and
    /// tool names; it never supplies what those are allowed to do.
    pub(super) fn with_declared_effects(&self, action: ExecutionAction) -> ExecutionAction {
        match action {
            ExecutionAction::Mcp {
                server,
                tool,
                arguments,
                effects: _,
            } => {
                let declared = self
                    .services
                    .server_effects(&server)
                    .unwrap_or(ProcessEffects {
                        // An unknown server gets the conservative
                        // all-effects declaration, exactly as N04's own
                        // doc comment on `ExecutionAction::Mcp` requires.
                        repo_write: true,
                        outside_write: true,
                        network: true,
                        git_metadata_write: true,
                        git_push_or_destructive: true,
                        clean_environment: false,
                    });
                ExecutionAction::Mcp {
                    server,
                    tool,
                    arguments,
                    effects: declared,
                }
            }
            other => other,
        }
    }

    pub(super) fn web(&self) -> Result<&super::super::capabilities::WebBackend, ToolError> {
        self.services.web.as_ref().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::PreconditionFailed,
                "no web capability is configured; see capabilities.web in ~/.zirv/ctx.toml",
            )
        })
    }

    pub(super) fn browser(&self) -> Result<&super::super::capabilities::BrowserBackend, ToolError> {
        self.services.browser.as_ref().ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::PreconditionFailed,
                "no browser capability is configured or discovered; see capabilities.browser in \
                 ~/.zirv/ctx.toml",
            )
        })
    }

    pub(super) fn capability_report(&self) -> Result<Value, ToolError> {
        Ok(json!({
            "integrations": self.services.integrations,
            "mcp_servers": self.services.server_names(),
            "registered_mcp_tools": self
                .registry
                .definitions()
                .filter(|definition| definition.name.starts_with(MCP_PREFIX))
                .map(|definition| definition.name.clone())
                .collect::<Vec<_>>(),
        }))
    }

    pub(super) fn present_artifact(&self, args: &ArtifactPresentArgs) -> Result<Value, ToolError> {
        use crate::commands::workflow::artifact;
        use crate::commands::workflow::capability::CapabilityReport;

        let record =
            artifact::load(&self.state, &self.repo, &args.id).map_err(ToolError::external)?;
        let report = CapabilityReport::for_repo("native", &self.repo)
            .map_err(ToolError::external)?
            .with_integrations(self.services.integrations.clone());
        let plan =
            artifact::presentation_plan("native", &record.path, args.interactive, false, &report)
                .map_err(ToolError::external)?;
        Ok(json!({
            "artifact": record,
            "plan": plan,
            "evidence_path": record.path.display().to_string(),
        }))
    }

    pub(super) fn list_mcp(&mut self, server: Option<&str>) -> Result<Value, ToolError> {
        let names = match server {
            Some(name) => vec![name.to_string()],
            None => self.services.server_names(),
        };
        let mut servers = Vec::new();
        for name in names {
            match self.services.client(&name, &self.broker) {
                Ok(client) => servers.push(json!({
                    "server": name,
                    "state": "available",
                    "info": client.info(),
                    "catalogue": client.catalogue().index(),
                })),
                Err(error) => servers.push(json!({
                    "server": name,
                    "state": "unavailable",
                    "diagnosis": error.to_string(),
                })),
            }
        }
        Ok(json!({ "servers": servers }))
    }

    pub(super) fn call_mcp(&mut self, args: &McpCallArgs) -> Result<Value, ToolError> {
        let arguments = if args.arguments.is_null() {
            json!({})
        } else {
            args.arguments.clone()
        };
        let result = self
            .services
            .client(&args.server, &self.broker)
            .map_err(ToolError::from)?
            .call_tool(
                &args.tool,
                arguments,
                &super::super::super::provider::adapter::NeverCancelled,
            )
            .map_err(mcp_error)?;
        let value = serde_json::to_value(&result).map_err(ToolError::external)?;
        self.bounded(value, "text", &["mcp_call", &args.server, &args.tool])
    }

    /// Moves one oversized string field of a result into the existing output
    /// store, leaving a bounded summary and the opaque retrieval id behind.
    /// The same "never let the summary be the only copy" rule process output
    /// already follows, applied to untrusted MCP and web payloads.
    pub(super) fn bounded(
        &self,
        mut value: Value,
        field: &str,
        command: &[&str],
    ) -> Result<Value, ToolError> {
        let Some(object) = value.as_object_mut() else {
            return Ok(value);
        };
        let Some(text) = object.get(field).and_then(Value::as_str) else {
            return Ok(value);
        };
        if text.len() <= self.limits.max_inline_bytes {
            return Ok(value);
        }
        let stored = persist_capture(
            &self.state,
            &self.repo,
            CapturePayload::new(
                text.as_bytes().to_vec(),
                command.iter().map(|part| (*part).to_string()).collect(),
                CompactionScope::Generic,
            ),
            self.limits.process.max_summary_bytes,
            &self.limits.process.output_filter,
        )?;
        let head: String = text
            .chars()
            .take(self.limits.max_inline_bytes / 2)
            .collect();
        object.insert(field.into(), Value::String(head));
        object.insert("truncated".into(), Value::Bool(true));
        object.insert("output_id".into(), Value::String(stored.id));
        object.insert(
            "summary".into(),
            stored.summary.map(Value::String).unwrap_or(Value::Null),
        );
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::enforcement::NetworkScope;
    use crate::commands::ctx::runtime::tools::tests::*;
    use crate::commands::ctx::runtime::tools::*;

    #[test]
    fn an_evidence_label_can_never_escape_its_own_directory() {
        for label in ["../../etc/passwd", "C:\\windows\\system32", "a/b/c", "\0"] {
            let slug = evidence_slug(label);
            assert!(!slug.contains(['/', '\\', '\0']), "{slug}");
            assert!(!slug.contains(".."), "{slug}");
        }
        assert_eq!(evidence_slug("Home page — wide"), "home-page-wide");
        assert_eq!(evidence_slug("---"), "capture");
    }

    #[test]
    fn only_absolute_http_urls_become_a_network_target() {
        assert!(network_target("https://docs.example/a").is_ok());
        assert!(network_target("http://docs.example").is_ok());
        for bad in ["file:///etc/passwd", "docs.example", "https://"] {
            assert!(network_target(bad).is_err(), "{bad} must be refused");
        }
    }

    fn entry(name: &str, property: &str) -> super::super::super::mcp::McpToolEntry {
        super::super::super::mcp::McpToolEntry {
            name: name.to_string(),
            title: None,
            summary: "A server-described tool.".into(),
            input_schema: json!({"type":"object","properties":{property:{"type":"string"}}}),
            digest: format!("digest-{name}-{property}"),
        }
    }

    #[test]
    fn every_capability_tool_parses_through_the_same_closed_registry() {
        let registry = ToolRegistry::native();
        for name in [
            WEB_SEARCH,
            WEB_FETCH,
            BROWSER_CAPTURE,
            BROWSER_INSPECT,
            DIAGNOSTICS_REPORT,
            CAPABILITY_REPORT,
            ARTIFACT_REGISTER,
            ARTIFACT_PRESENT,
            FRONTEND_RENDER,
            FRONTEND_REVIEW,
            MCP_LIST,
            MCP_DESCRIBE,
            MCP_CALL,
        ] {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(definition.input_schema["additionalProperties"], false);
        }
        let extra = registry
            .parse(WEB_SEARCH, json!({"query":"a","depth":3}))
            .expect_err("unknown fields are rejected");
        assert_eq!(extra.code, ToolErrorCode::InvalidArguments);
        let empty = registry
            .parse(
                BROWSER_CAPTURE,
                json!({"url":"https://a.example","label":""}),
            )
            .expect_err("an empty label is rejected");
        assert_eq!(empty.code, ToolErrorCode::InvalidArguments);
    }

    #[test]
    fn artifact_register_accepts_the_video_kind() {
        ToolRegistry::native()
            .parse(
                ARTIFACT_REGISTER,
                json!({"path":"renders/demo.mp4","kind":"video"}),
            )
            .expect("video is a registrable kind");
    }

    #[test]
    fn a_web_or_browser_tool_becomes_a_host_scoped_network_action() {
        let registry = ToolRegistry::native();
        let parsed = registry
            .parse(WEB_FETCH, json!({"url":"https://Docs.Example:443/a?b=c"}))
            .expect("parse");
        let ExecutionAction::Network { target } = parsed.action().expect("action") else {
            panic!("web_fetch must be a network action");
        };
        assert_eq!(target.host, "docs.example");
        assert_eq!(target.scheme, "https");

        let bad = registry
            .parse(BROWSER_INSPECT, json!({"url":"file:///etc/passwd"}))
            .expect("parse")
            .action()
            .expect_err("a non-http URL must never become an action");
        assert_eq!(bad.code, ToolErrorCode::InvalidArguments);
    }

    #[test]
    fn a_promoted_mcp_tool_is_namespaced_and_can_never_shadow_a_built_in() {
        let mut registry = ToolRegistry::native();
        let name = registry
            .register_mcp(
                "docs",
                &entry("file_write", "path"),
                ProcessEffects::default(),
            )
            .expect("register");
        assert_eq!(name, "mcp__docs__file_write");
        assert!(
            registry.get(FILE_WRITE).is_some(),
            "the built-in file_write must be untouched"
        );
        let parsed = registry
            .parse(&name, json!({"path":"docs/a.md"}))
            .expect("parse");
        let ExecutionAction::Mcp { server, tool, .. } = parsed.action().expect("action") else {
            panic!("a promoted tool must be an MCP action");
        };
        assert_eq!((server.as_str(), tool.as_str()), ("docs", "file_write"));
    }

    #[test]
    fn a_promoted_mcp_tool_declares_only_the_effects_its_operator_configured() {
        let mut registry = ToolRegistry::native();
        let name = registry
            .register_mcp(
                "deploy",
                &entry("ship", "target"),
                ProcessEffects {
                    network: true,
                    git_push_or_destructive: true,
                    ..ProcessEffects::default()
                },
            )
            .expect("register");
        let definition = registry.get(&name).expect("definition");
        assert!(definition.capabilities.contains(&"network".to_string()));
        assert!(
            definition
                .capabilities
                .contains(&"git_push_destructive".to_string())
        );
        assert!(
            !definition
                .capabilities
                .contains(&"repo_fs_write".to_string()),
            "an undeclared effect must not be granted"
        );
        assert_eq!(definition.retry, RetryPolicy::NeverAfterStart);
    }

    #[test]
    fn clearing_a_server_removes_only_its_own_promoted_tools() {
        let mut registry = ToolRegistry::native();
        registry
            .register_mcp(
                "docs",
                &entry("lookup", "symbol"),
                ProcessEffects::default(),
            )
            .expect("register");
        registry
            .register_mcp(
                "other",
                &entry("lookup", "symbol"),
                ProcessEffects::default(),
            )
            .expect("register");
        registry.clear_mcp("docs");
        assert!(registry.get("mcp__docs__lookup").is_none());
        assert!(registry.get("mcp__other__lookup").is_some());
        assert!(registry.binding("mcp__docs__lookup").is_none());
        assert_eq!(
            registry
                .parse("mcp__docs__lookup", json!({}))
                .expect_err("gone")
                .code,
            ToolErrorCode::UnknownTool
        );
    }

    #[test]
    fn an_mcp_call_never_carries_a_retry_policy_that_would_replay_a_remote_effect() {
        let parsed = ToolRegistry::native()
            .parse(MCP_CALL, json!({"server":"docs","tool":"ship"}))
            .expect("parse");
        assert_eq!(parsed.retry_policy(), RetryPolicy::NeverAfterStart);
        let discovery = ToolRegistry::native()
            .parse(MCP_LIST, json!({}))
            .expect("parse");
        assert_eq!(discovery.retry_policy(), RetryPolicy::Safe);
    }

    struct EndToEnd {
        _root: tempfile::TempDir,
        client: NativeToolClient,
    }

    fn end_to_end(
        policy: super::super::super::super::policy::EffectivePolicy,
        server: super::super::super::mcp::FixtureServer,
        max_inline_mcp_tools: usize,
    ) -> EndToEnd {
        end_to_end_configured(
            policy,
            server,
            max_inline_mcp_tools,
            Default::default(),
            NetworkScope::Any,
        )
    }

    fn end_to_end_with_effects(
        policy: super::super::super::super::policy::EffectivePolicy,
        server: super::super::super::mcp::FixtureServer,
        max_inline_mcp_tools: usize,
        effects: super::super::super::super::config::CapabilityEffectsConfig,
    ) -> EndToEnd {
        end_to_end_configured(
            policy,
            server,
            max_inline_mcp_tools,
            effects,
            NetworkScope::Any,
        )
    }

    fn end_to_end_with_network_scope(
        policy: super::super::super::super::policy::EffectivePolicy,
        server: super::super::super::mcp::FixtureServer,
        max_inline_mcp_tools: usize,
        network: super::super::super::enforcement::NetworkScope,
    ) -> EndToEnd {
        end_to_end_configured(
            policy,
            server,
            max_inline_mcp_tools,
            Default::default(),
            network,
        )
    }

    fn end_to_end_configured(
        policy: super::super::super::super::policy::EffectivePolicy,
        server: super::super::super::mcp::FixtureServer,
        max_inline_mcp_tools: usize,
        effects: super::super::super::super::config::CapabilityEffectsConfig,
        network: NetworkScope,
    ) -> EndToEnd {
        use super::super::super::super::config::{
            CapabilitiesConfig, CtxConfig, McpServerConfig, McpTransportConfig,
        };
        use super::super::super::enforcement::{
            ApprovalAuthority, ApprovalMode, ExecutionBroker, ExecutionIdentity, PlatformIsolation,
            PolicySnapshot, ResourceClaims,
        };

        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        let state_root = root.path().join("state");
        let home = root.path().join("home");
        for path in [&repo, &state_root, &home] {
            std::fs::create_dir_all(path).expect("create root");
        }
        let repo = std::fs::canonicalize(&repo).expect("canonical repo");
        let claims =
            ResourceClaims::new(&repo, &repo, &state_root, &home, network).expect("claims");
        let writer = effects.repo_write.then(|| {
            Box::new(FixtureWriter(repo.clone()))
                as Box<dyn crate::commands::ctx::runtime::enforcement::WriterLease>
        });
        let broker = ExecutionBroker::new(
            ExecutionIdentity {
                session: "session-483".into(),
                short: "abcd1234".into(),
                generation: 1,
                role: "worker".into(),
                task: None,
            },
            claims,
            ApprovalMode::Headless,
            std::sync::Arc::new(FixedPolicy(
                PolicySnapshot::new(
                    policy,
                    super::super::super::super::safety::SafetyPolicy::default(),
                )
                .expect("policy"),
            )),
            std::sync::Arc::new(FixedFence),
            std::sync::Arc::new(ApprovalAuthority::new()),
            writer,
            PlatformIsolation::Unavailable {
                platform: "test".into(),
                reason: "no test sandbox".into(),
            },
            Default::default(),
        )
        .expect("broker");

        let cfg = CtxConfig {
            capabilities: CapabilitiesConfig {
                enabled: true,
                max_inline_mcp_tools,
                mcp: vec![McpServerConfig {
                    name: "docs".into(),
                    enabled: true,
                    transport: McpTransportConfig::Stdio {
                        command: "never-spawned".into(),
                        args: Vec::new(),
                        cwd: None,
                        environment: Default::default(),
                    },
                    effects,
                    ..McpServerConfig::default()
                }],
                ..CapabilitiesConfig::default()
            },
            ..CtxConfig::default()
        };
        let mut services =
            super::super::super::capabilities::CapabilityServices::for_servers(&cfg, &repo);
        services.transport_overrides.insert(
            "docs".into(),
            std::sync::Arc::new(super::super::super::mcp::FixtureFactory::new(server)),
        );
        let client = NativeToolClient::new(
            broker,
            StateDir::from_root(state_root.clone()),
            repo,
            ToolLimits::testing(),
        )
        .with_capabilities(services);
        EndToEnd {
            _root: root,
            client,
        }
    }

    fn tool_row(name: &str) -> Value {
        json!({
            "name": name,
            "description": "A tool a server described.",
            "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}},
        })
    }

    #[test]
    fn a_native_session_invokes_an_mcp_server_through_the_broker_with_a_bounded_receipt() {
        let mut fixture = end_to_end(
            super::super::super::super::policy::EffectivePolicy::default(),
            super::super::super::mcp::FixtureServer {
                tools: vec![tool_row("lookup")],
                ..Default::default()
            },
            24,
        );
        let receipt =
            fixture
                .client
                .execute("mcp__docs__lookup", json!({"value": "x"}), None, None);
        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
        assert_eq!(receipt.retry, RetryPolicy::NeverAfterStart);
        assert!(receipt.policy_fingerprint.is_some());
        let result = receipt.result.expect("result");
        assert_eq!(result["text"], "ran lookup");
        assert_eq!(result["is_error"], false);
    }

    #[test]
    fn policy_denial_stops_an_mcp_call_before_the_server_is_ever_reached() {
        use super::super::super::super::policy::{EffectivePolicy, Stance};

        let mut fixture = end_to_end(
            EffectivePolicy {
                tool_access: Stance::Deny,
                ..EffectivePolicy::default()
            },
            super::super::super::mcp::FixtureServer {
                tools: vec![tool_row("lookup")],
                ..Default::default()
            },
            24,
        );
        let receipt =
            fixture
                .client
                .execute("mcp__docs__lookup", json!({"value": "x"}), None, None);
        assert_eq!(receipt.state, ToolReceiptState::Failed);
        assert_eq!(
            receipt.error.expect("error").code,
            ToolErrorCode::AuthorizationDenied
        );
    }

    #[test]
    // Issue #566: server-controlled names cannot suppress configured effects.
    fn mcp_tool_named_tools_prefix_still_uses_declared_effects() {
        use super::super::super::super::config::CapabilityEffectsConfig;
        use super::super::super::super::policy::{EffectivePolicy, Stance};

        let effects = CapabilityEffectsConfig {
            repo_write: true,
            network: true,
            ..CapabilityEffectsConfig::default()
        };
        for policy in [
            EffectivePolicy {
                repo_fs_write: Stance::Deny,
                network: Some(Stance::Allow),
                ..EffectivePolicy::default()
            },
            EffectivePolicy {
                repo_fs_write: Stance::Allow,
                network: Some(Stance::Deny),
                ..EffectivePolicy::default()
            },
        ] {
            let mut fixture = end_to_end_with_effects(
                policy,
                super::super::super::mcp::FixtureServer {
                    tools: vec![tool_row("tools/poison")],
                    fail_next_call: true,
                    ..Default::default()
                },
                24,
                effects.clone(),
            );
            let receipt = fixture.client.execute(
                MCP_CALL,
                json!({"server":"docs","tool":"tools/poison","arguments":{}}),
                None,
                None,
            );
            assert_eq!(receipt.state, ToolReceiptState::Failed, "{receipt:?}");
            assert_eq!(
                receipt.error.expect("policy denial").code,
                ToolErrorCode::AuthorizationDenied
            );
        }
    }

    #[test]
    fn a_large_catalogue_stays_out_of_the_registry_and_is_reachable_by_index() {
        let tools: Vec<Value> = (0..40).map(|i| tool_row(&format!("tool{i}"))).collect();
        let mut fixture = end_to_end(
            super::super::super::super::policy::EffectivePolicy::default(),
            super::super::super::mcp::FixtureServer {
                tools,
                ..Default::default()
            },
            8,
        );
        assert!(
            !fixture
                .client
                .registry()
                .definitions()
                .any(|definition| definition.name.starts_with(MCP_PREFIX)),
            "a catalogue above the inline budget must not be promoted"
        );
        let receipt = fixture.client.execute(MCP_LIST, json!({}), None, None);
        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
        let result = receipt.result.expect("result");
        assert_eq!(result["servers"][0]["catalogue"]["count"], 40);
        assert!(
            result["servers"][0]["catalogue"]["tools"][0]["input_schema"].is_null(),
            "the index must not carry schemas"
        );

        // The same tool is still callable by name through mcp_call.
        let receipt = fixture.client.execute(
            MCP_CALL,
            json!({"server": "docs", "tool": "tool7", "arguments": {"value": "x"}}),
            None,
            None,
        );
        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
    }

    #[test]
    fn an_unconfigured_web_capability_fails_the_call_rather_than_returning_nothing() {
        let mut fixture = end_to_end(
            super::super::super::super::policy::EffectivePolicy {
                network: Some(super::super::super::super::policy::Stance::Allow),
                ..super::super::super::super::policy::EffectivePolicy::default()
            },
            super::super::super::mcp::FixtureServer::default(),
            24,
        );
        let receipt = fixture
            .client
            .execute(WEB_SEARCH, json!({"query": "rust"}), None, None);
        assert_eq!(receipt.state, ToolReceiptState::Failed);
        let error = receipt.error.expect("error");
        assert_eq!(error.code, ToolErrorCode::PreconditionFailed);
        assert!(error.message.contains("capabilities.web"), "{error:?}");
    }

    #[test]
    fn network_denied_role_cannot_use_web_search() {
        // Issue #558.
        use super::super::super::super::config::WebCapabilityConfig;
        use super::super::super::super::policy::{EffectivePolicy, Stance};
        use super::super::super::capabilities::{
            HttpGetReply, HttpGetter, PassThroughEgress, WebBackend,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct CountingGetter(std::sync::Arc<AtomicUsize>);

        impl HttpGetter for CountingGetter {
            fn get(
                &self,
                _: &str,
                _: &[(String, String)],
                _: usize,
                _: std::time::Duration,
            ) -> Result<HttpGetReply, String> {
                self.0.fetch_add(1, Ordering::AcqRel);
                Ok(HttpGetReply {
                    status: 200,
                    content_type: "application/json".into(),
                    body: r#"{"results":[]}"#.into(),
                })
            }
        }

        let requests = std::sync::Arc::new(AtomicUsize::new(0));
        let mut fixture = end_to_end(
            EffectivePolicy {
                network: Some(Stance::Deny),
                ..EffectivePolicy::default()
            },
            super::super::super::mcp::FixtureServer::default(),
            24,
        );
        fixture.client.services.web = Some(WebBackend::new(
            WebCapabilityConfig {
                search_endpoint: Some("https://search.example/?q={query}".into()),
                allow_hosts: vec!["search.example".into()],
                ..WebCapabilityConfig::default()
            },
            None,
            std::sync::Arc::new(CountingGetter(std::sync::Arc::clone(&requests))),
            std::sync::Arc::new(PassThroughEgress),
        ));
        let receipt = fixture
            .client
            .execute(WEB_SEARCH, json!({"query":"blocked"}), None, None);
        assert_eq!(receipt.state, ToolReceiptState::Failed);
        assert_eq!(
            receipt.error.expect("policy error").code,
            ToolErrorCode::AuthorizationDenied
        );
        assert_eq!(requests.load(Ordering::Acquire), 0);
    }

    #[test]
    fn host_scoped_role_cannot_use_browser_tools() {
        // Issue #558.
        use super::super::super::super::policy::{EffectivePolicy, Stance};
        use super::super::super::enforcement::{NetworkScope, NetworkTarget};

        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        for (tool, arguments) in [
            (
                BROWSER_CAPTURE,
                json!({"url":"https://example.com","label":"page"}),
            ),
            (BROWSER_INSPECT, json!({"url":"https://example.com"})),
        ] {
            let target = NetworkTarget::new("https", "example.com", None).expect("target");
            for (scope, label) in [
                (NetworkScope::Denied, "denied"),
                (
                    NetworkScope::Only {
                        targets: std::iter::once(target.clone()).collect(),
                    },
                    "only",
                ),
            ] {
                let mut fixture = end_to_end_with_network_scope(
                    policy.clone(),
                    super::super::super::mcp::FixtureServer::default(),
                    24,
                    scope,
                );
                let receipt = fixture.client.execute(tool, arguments.clone(), None, None);
                assert_eq!(receipt.state, ToolReceiptState::Failed);
                let error = receipt.error.expect("scope error");
                assert_eq!(error.code, ToolErrorCode::AuthorizationDenied);
                assert!(error.message.contains(tool), "{error:?}");
                assert!(error.message.contains(label), "{error:?}");
                assert!(error.message.contains("request interception"), "{error:?}");
                assert!(error.message.contains("not shipped yet"), "{error:?}");
            }

            let mut fixture = end_to_end(
                policy.clone(),
                super::super::super::mcp::FixtureServer::default(),
                24,
            );
            let receipt = fixture.client.execute(tool, arguments, None, None);
            assert_ne!(
                receipt.error.map(|error| error.code),
                Some(ToolErrorCode::AuthorizationDenied)
            );
        }
    }

    #[test]
    fn the_capability_report_tool_names_every_integration_state() {
        let mut fixture = end_to_end(
            super::super::super::super::policy::EffectivePolicy::default(),
            super::super::super::mcp::FixtureServer {
                tools: vec![tool_row("lookup")],
                ..Default::default()
            },
            24,
        );
        let receipt = fixture
            .client
            .execute(CAPABILITY_REPORT, json!({}), None, None);
        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
        let result = receipt.result.expect("result");
        let states: Vec<&str> = result["integrations"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| row["state"].as_str().unwrap_or_default())
            .collect();
        assert!(
            states
                .iter()
                .all(|state| ["available", "unavailable", "unverified"].contains(state)),
            "{states:?}"
        );
        assert_eq!(
            result["registered_mcp_tools"],
            json!(["mcp__docs__lookup"]),
            "a small catalogue is promoted and reported"
        );
    }

    #[test]
    fn a_cancelled_mcp_call_reports_an_unknown_outcome_rather_than_a_clean_failure() {
        let error = mcp_error(super::super::super::mcp::McpError::Cancelled);
        assert!(error.outcome_unknown);
        let stale = mcp_error(super::super::super::mcp::McpError::StaleTool(
            "changed".into(),
        ));
        assert_eq!(stale.code, ToolErrorCode::PreconditionFailed);
        assert!(!stale.outcome_unknown);
    }

    #[cfg(unix)]
    #[test]
    fn mcp_disconnect_after_request_write_records_outcome_unknown() {
        // Issue #569.
        use super::super::super::super::config::{McpServerConfig, McpTransportConfig};
        use super::super::super::super::provider::{
            AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
        };
        use super::super::super::journal::{
            PolicyProvenance, RouteIdentity, SeatId, SessionIdentity,
        };
        use super::super::super::mcp::{McpError, StdioTransport, TransportFactory};
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct DisconnectFactory {
            connects: AtomicUsize,
            marker: std::path::PathBuf,
        }

        impl TransportFactory for DisconnectFactory {
            fn connect(&self) -> Result<Box<dyn super::super::super::mcp::McpTransport>, McpError> {
                let disconnect = self.connects.fetch_add(1, Ordering::AcqRel) == 0;
                let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      id=${line#*\"id\":}; id=${id%%,*}
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{\"tools\":{},\"resources\":{}},\"serverInfo\":{\"name\":\"disconnect\",\"version\":\"1\"}}}"
      ;;
    *'"method":"tools/list"'*)
      id=${line#*\"id\":}; id=${id%%,*}
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"mutate\",\"description\":\"change state\",\"inputSchema\":{\"type\":\"object\"}}]}}"
      ;;
    *'"method":"resources/list"'*)
      id=${line#*\"id\":}; id=${id%%,*}
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"resources\":[]}}"
      ;;
    *'"method":"tools/call"'*)
      printf received > "$MCP_DISCONNECT_MARKER"
      if [ "$MCP_DISCONNECT" = yes ]; then exit 0; fi
      id=${line#*\"id\":}; id=${id%%,*}
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"mutated\"}]}}"
      ;;
  esac
done
"#;
                let config = McpServerConfig {
                    name: "disconnect".into(),
                    transport: McpTransportConfig::Stdio {
                        command: "/bin/sh".into(),
                        args: vec!["-c".into(), script.into()],
                        cwd: None,
                        environment: std::collections::BTreeMap::from([
                            (
                                "MCP_DISCONNECT".into(),
                                if disconnect { "yes" } else { "no" }.into(),
                            ),
                            (
                                "MCP_DISCONNECT_MARKER".into(),
                                self.marker.to_string_lossy().into_owned(),
                            ),
                        ]),
                    },
                    ..McpServerConfig::default()
                };
                Ok(Box::new(StdioTransport::spawn(&config)?))
            }
        }

        let mut fixture = end_to_end(
            super::super::super::super::policy::EffectivePolicy::default(),
            super::super::super::mcp::FixtureServer {
                tools: vec![tool_row("mutate")],
                ..Default::default()
            },
            24,
        );
        fixture.client.services.shutdown();
        let marker = fixture.client.state.root().join("mcp-disconnect-received");
        fixture.client.services.transport_overrides.insert(
            "docs".into(),
            std::sync::Arc::new(DisconnectFactory {
                connects: AtomicUsize::new(0),
                marker: marker.clone(),
            }),
        );

        let session = JournalSessionId::new("session-569").expect("session id");
        let mut journal = Journal::open(&fixture.client.state).expect("journal");
        journal
            .create_session(&SessionIdentity {
                session: session.clone(),
                seat: SeatId::new("seat-569").expect("seat id"),
                generation: 1,
                task: None,
                route: RouteIdentity {
                    route: RouteId::new("route-569").expect("route id"),
                    provider: ProviderId::new("fixture").expect("provider id"),
                    endpoint: EndpointId::new("fixture").expect("endpoint id"),
                    account: AccountId::new("fixture").expect("account id"),
                    billing_pool: BillingPoolId::new("fixture").expect("pool id"),
                    protocol: Protocol::OpenAiResponses,
                    model: ModelId {
                        vendor: "fixture".into(),
                        id: "fixture-model".into(),
                    },
                },
                repo: std::path::PathBuf::from("/native-test-repo"),
                created_at: 1,
                completed_at: None,
            })
            .expect("create session");
        let scope = EventScope {
            turn: None,
            attempt: None,
            task: None,
        };
        let call = ToolCallId::new("call-569").expect("call id");
        let execution = ExecutionId::new("execution-569").expect("execution id");
        journal
            .prepare_tool_call(
                &session,
                1,
                &scope,
                call.clone(),
                "mcp__docs__mutate".into(),
                json!({"value":"x"}),
                PolicyProvenance {
                    fingerprint: "fixture".into(),
                    source: "test".into(),
                    decision: "allow".into(),
                    scope: "test".into(),
                },
                Some(1),
                1,
            )
            .expect("prepare call");
        let receipt = fixture.client.execute(
            "mcp__docs__mutate",
            json!({"value":"x"}),
            None,
            Some(JournalExecution {
                journal: &mut journal,
                session: session.clone(),
                generation: 1,
                scope,
                tool_call: call,
                execution: execution.clone(),
            }),
        );
        assert_eq!(
            receipt.state,
            ToolReceiptState::OutcomeUnknown,
            "{receipt:?}"
        );
        let guidance = &receipt
            .error
            .as_ref()
            .expect("uncertainty guidance")
            .message;
        assert!(guidance.contains("reconcile"), "{guidance}");
        assert!(!guidance.contains("re-issue"), "{guidance}");
        assert_eq!(
            std::fs::read_to_string(marker).expect("request marker"),
            "received"
        );
        assert_eq!(
            journal.replay(&session).expect("replay").executions[&execution].state,
            ExecutionState::OutcomeUnknown
        );
    }
}
