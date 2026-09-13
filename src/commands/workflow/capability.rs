//! Logical capabilities requested by skills and workflows.
//!
//! A capability report describes what Zirv can arrange on a harness. It is
//! not an authorization grant. The canonical policy work in issue #43 can
//! narrow these reports through [`PolicyDecision`] without changing skill
//! manifests or teaching them provider tool names.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::policy::{Capability as PolicyCapability, EffectivePolicy, Stance};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CapabilityId {
    #[serde(rename = "shell.exec")]
    ShellExec,
    #[serde(rename = "repo.read")]
    RepoRead,
    #[serde(rename = "repo.write")]
    RepoWrite,
    #[serde(rename = "git.worktree")]
    GitWorktree,
    #[serde(rename = "agent.spawn")]
    AgentSpawn,
    #[serde(rename = "test.run")]
    TestRun,
    #[serde(rename = "artifact.render")]
    ArtifactRender,
    #[serde(rename = "browser.open")]
    BrowserOpen,
    #[serde(rename = "network.access")]
    NetworkAccess,
}

impl CapabilityId {
    pub const ALL: [Self; 9] = [
        Self::ShellExec,
        Self::RepoRead,
        Self::RepoWrite,
        Self::GitWorktree,
        Self::AgentSpawn,
        Self::TestRun,
        Self::ArtifactRender,
        Self::BrowserOpen,
        Self::NetworkAccess,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShellExec => "shell.exec",
            Self::RepoRead => "repo.read",
            Self::RepoWrite => "repo.write",
            Self::GitWorktree => "git.worktree",
            Self::AgentSpawn => "agent.spawn",
            Self::TestRun => "test.run",
            Self::ArtifactRender => "artifact.render",
            Self::BrowserOpen => "browser.open",
            Self::NetworkAccess => "network.access",
        }
    }
}

impl std::fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportLevel {
    Supported,
    Degraded,
    Unsupported,
    OperatorControlled,
}

impl SupportLevel {
    pub fn satisfies_requirement(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

impl std::fmt::Display for SupportLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Supported => "supported",
            Self::Degraded => "degraded",
            Self::Unsupported => "unsupported",
            Self::OperatorControlled => "operator-controlled",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityStatus {
    pub capability: CapabilityId,
    pub support: SupportLevel,
    pub authorization: PolicyDecision,
    pub reason: String,
}

/// One concrete integration a native session can be equipped with (issue
/// #483, roadmap N14). Distinct from [`CapabilityId`] on purpose:
/// a `CapabilityId` is a logical permission a skill asks for, while an
/// `IntegrationId` is a real backend that either exists on this machine or
/// does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IntegrationId {
    #[serde(rename = "mcp")]
    Mcp,
    #[serde(rename = "web.search")]
    WebSearch,
    #[serde(rename = "web.fetch")]
    WebFetch,
    #[serde(rename = "browser")]
    Browser,
    #[serde(rename = "diagnostics")]
    Diagnostics,
    #[serde(rename = "artifact.render")]
    ArtifactRender,
    #[serde(rename = "frontend.render")]
    FrontendRender,
}

impl IntegrationId {
    pub const ALL: [Self; 7] = [
        Self::Mcp,
        Self::WebSearch,
        Self::WebFetch,
        Self::Browser,
        Self::Diagnostics,
        Self::ArtifactRender,
        Self::FrontendRender,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::WebSearch => "web.search",
            Self::WebFetch => "web.fetch",
            Self::Browser => "browser",
            Self::Diagnostics => "diagnostics",
            Self::ArtifactRender => "artifact.render",
            Self::FrontendRender => "frontend.render",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|integration| integration.as_str() == value)
    }
}

impl std::fmt::Display for IntegrationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three honest answers, and only these three. `Unverified` is what keeps
/// the other two truthful: a configured MCP server that has not been
/// contacted this run is not evidence that it answers, and calling it
/// `Available` would be exactly the "incomplete native support reported as
/// full parity" this roadmap forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationState {
    Available,
    Unavailable,
    Unverified,
}

impl IntegrationState {
    /// Whether a workflow step requiring this integration may be entered. An
    /// unverified integration is admitted -- it may well work -- but an
    /// unavailable one is refused before the step starts.
    pub fn admits_step(self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

impl std::fmt::Display for IntegrationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Unverified => "unverified",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationStatus {
    pub integration: IntegrationId,
    pub state: IntegrationState,
    /// What was actually found: the backend, the binary, the server names.
    pub detail: String,
    /// For anything but `Available`: the exact missing binary, credential or
    /// config key, phrased so an operator can act on it without reading code.
    pub diagnosis: Option<String>,
}

impl IntegrationStatus {
    pub fn available(integration: IntegrationId, detail: impl Into<String>) -> Self {
        Self {
            integration,
            state: IntegrationState::Available,
            detail: detail.into(),
            diagnosis: None,
        }
    }

    pub fn unavailable(
        integration: IntegrationId,
        detail: impl Into<String>,
        diagnosis: impl Into<String>,
    ) -> Self {
        Self {
            integration,
            state: IntegrationState::Unavailable,
            detail: detail.into(),
            diagnosis: Some(diagnosis.into()),
        }
    }

    pub fn unverified(
        integration: IntegrationId,
        detail: impl Into<String>,
        diagnosis: impl Into<String>,
    ) -> Self {
        Self {
            integration,
            state: IntegrationState::Unverified,
            detail: detail.into(),
            diagnosis: Some(diagnosis.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityReport {
    pub adapter: String,
    pub statuses: Vec<CapabilityStatus>,
    /// Issue #483: the concrete integrations discovered for this repository.
    /// Empty on a report built without discovery (`for_adapter`), which is
    /// why [`CapabilityReport::admit`] treats an absent row as unavailable
    /// rather than as permission.
    #[serde(default)]
    pub integrations: Vec<IntegrationStatus>,
}

impl CapabilityReport {
    /// Honest baseline for current adapters. Filesystem/shell/network
    /// permissions remain operator-controlled because markdown cannot enforce
    /// them. Zirv-owned operations can be reported as supported independently
    /// of a vendor's native tool vocabulary.
    pub fn for_adapter(adapter: &str) -> Self {
        let known = matches!(adapter, "claude" | "codex");
        let status = |capability, support, reason: &'static str| CapabilityStatus {
            capability,
            support,
            authorization: PolicyDecision::Allow,
            reason: reason.to_string(),
        };
        let statuses = if known {
            vec![
                status(
                    CapabilityId::ShellExec,
                    SupportLevel::OperatorControlled,
                    "the harness and operator policy control shell access",
                ),
                status(
                    CapabilityId::RepoRead,
                    SupportLevel::OperatorControlled,
                    "the harness and operator policy control repository reads",
                ),
                status(
                    CapabilityId::RepoWrite,
                    SupportLevel::OperatorControlled,
                    "the harness and operator policy control repository writes",
                ),
                status(
                    CapabilityId::GitWorktree,
                    SupportLevel::Degraded,
                    "available through shell execution; no native adapter operation",
                ),
                status(
                    CapabilityId::AgentSpawn,
                    SupportLevel::Supported,
                    "provided by Zirv supervision",
                ),
                status(
                    CapabilityId::TestRun,
                    SupportLevel::Supported,
                    "provided by Zirv's deterministic verification runner",
                ),
                status(
                    CapabilityId::ArtifactRender,
                    SupportLevel::Supported,
                    "provided by Zirv's artifact registry and static fallback",
                ),
                status(
                    CapabilityId::BrowserOpen,
                    SupportLevel::Degraded,
                    "available only when a browser-capable harness is configured",
                ),
                status(
                    CapabilityId::NetworkAccess,
                    SupportLevel::OperatorControlled,
                    "network access is controlled outside skill instructions",
                ),
            ]
        } else {
            CapabilityId::ALL
                .into_iter()
                .map(|capability| {
                    status(
                        capability,
                        SupportLevel::Unsupported,
                        "no capability mapping exists for this adapter",
                    )
                })
                .collect()
        };
        Self {
            adapter: adapter.to_string(),
            statuses,
            integrations: Vec::new(),
        }
    }

    pub fn with_integrations(mut self, integrations: Vec<IntegrationStatus>) -> Self {
        self.integrations = integrations;
        self
    }

    pub fn integration(&self, integration: IntegrationId) -> IntegrationState {
        self.integrations
            .iter()
            .find(|status| status.integration == integration)
            .map_or(IntegrationState::Unavailable, |status| status.state)
    }

    pub fn integration_status(&self, integration: IntegrationId) -> Option<&IntegrationStatus> {
        self.integrations
            .iter()
            .find(|status| status.integration == integration)
    }

    /// Workflow admission: a step whose required integration is unavailable is
    /// refused BEFORE it starts, and the refusal quotes the diagnosis, so the
    /// operator reads "`chromium` is not installed" rather than watching a
    /// step fail halfway through for reasons it has to reconstruct.
    pub fn admit(&self, required: &[IntegrationId]) -> Result<(), String> {
        for integration in required {
            let state = self.integration(*integration);
            if state.admits_step() {
                continue;
            }
            let diagnosis = self
                .integration_status(*integration)
                .and_then(|status| status.diagnosis.clone())
                .unwrap_or_else(|| "no capability discovery ran for this report".to_string());
            return Err(format!(
                "this step requires the `{integration}` integration, which is unavailable: \
                 {diagnosis}"
            ));
        }
        Ok(())
    }

    pub fn support(&self, capability: CapabilityId) -> SupportLevel {
        self.statuses
            .iter()
            .find(|status| status.capability == capability)
            .map(|status| status.support)
            .unwrap_or(SupportLevel::Unsupported)
    }

    pub fn authorization(&self, capability: CapabilityId) -> PolicyDecision {
        self.statuses
            .iter()
            .find(|status| status.capability == capability)
            .map(|status| status.authorization)
            .unwrap_or(PolicyDecision::Deny)
    }

    /// Resolve logical workflow capabilities against the effective canonical
    /// policy for `repo`. Policy loading uses the same asymmetric operator /
    /// repository fold as every AI launch, so repository content can narrow
    /// permissions but cannot grant itself a capability.
    /// Resolved report for `repo`: logical capabilities folded through the
    /// canonical policy, plus the concrete integrations auto-discovered
    /// within the authorization that already exists (issue #483). Discovery
    /// contacts nothing -- it reads config, PATH and the tree -- so this stays
    /// cheap enough for every workflow admission check.
    pub fn for_repo(adapter: &str, repo: &Path) -> CtxResult<Self> {
        let config =
            crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())?;
        let integrations = crate::commands::ctx::runtime::capabilities::discover(&config, repo);
        Ok(Self::for_policy(adapter, &config.policy).with_integrations(integrations))
    }

    pub fn for_policy(adapter: &str, policy: &EffectivePolicy) -> Self {
        Self::for_adapter(adapter).with_policy(|capability| policy_decision(policy, capability))
    }

    /// Policy may narrow support, never widen it. `Ask` remains explicitly
    /// operator-controlled; `Deny` is unsupported for this run.
    pub fn with_policy(mut self, policy: impl Fn(CapabilityId) -> PolicyDecision) -> Self {
        for status in &mut self.statuses {
            let decision = policy(status.capability);
            status.authorization = decision;
            status.support = match decision {
                PolicyDecision::Deny => SupportLevel::Unsupported,
                PolicyDecision::Ask if status.support.satisfies_requirement() => {
                    SupportLevel::OperatorControlled
                }
                PolicyDecision::Allow => status.support,
                PolicyDecision::Ask => status.support,
            };
            match decision {
                PolicyDecision::Deny => {
                    status.reason = "denied by Zirv's effective canonical policy".into();
                }
                PolicyDecision::Ask if status.support.satisfies_requirement() => {
                    status.reason =
                        "requires operator approval under Zirv's effective canonical policy".into();
                }
                PolicyDecision::Allow | PolicyDecision::Ask => {}
            }
        }
        self
    }
}

/// The integrations one workflow step genuinely cannot proceed without
/// (issue #483). Deliberately short: a step is refused only where the missing
/// backend makes the step impossible rather than merely harder. A frontend
/// step that has to render and inspect a page needs a browser; nothing else
/// in the ladder does.
pub fn required_integrations(
    phase: super::skill::WorkflowPhase,
    frontend_domain: bool,
) -> Vec<IntegrationId> {
    use super::skill::WorkflowPhase;

    match (phase, frontend_domain) {
        (WorkflowPhase::Implement | WorkflowPhase::Review | WorkflowPhase::Verify, true) => {
            vec![IntegrationId::FrontendRender]
        }
        (WorkflowPhase::Present, true) => {
            vec![IntegrationId::ArtifactRender, IntegrationId::FrontendRender]
        }
        (WorkflowPhase::Present, false) => vec![IntegrationId::ArtifactRender],
        _ => Vec::new(),
    }
}

fn policy_decision(policy: &EffectivePolicy, capability: CapabilityId) -> PolicyDecision {
    let relevant: &[PolicyCapability] = match capability {
        CapabilityId::ShellExec | CapabilityId::TestRun | CapabilityId::BrowserOpen => {
            &[PolicyCapability::ShellExec]
        }
        CapabilityId::RepoWrite | CapabilityId::ArtifactRender => &[PolicyCapability::RepoFsWrite],
        CapabilityId::GitWorktree => &[PolicyCapability::ShellExec, PolicyCapability::RepoFsWrite],
        CapabilityId::NetworkAccess => &[PolicyCapability::Network],
        CapabilityId::RepoRead | CapabilityId::AgentSpawn => &[],
    };
    let stance = relevant
        .iter()
        .map(|capability| policy.stance(*capability))
        .max()
        .unwrap_or(Stance::Allow);
    match stance {
        Stance::Allow => PolicyDecision::Allow,
        Stance::Ask => PolicyDecision::Ask,
        Stance::Deny => PolicyDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_adapters_resolve_the_same_logical_capability_names() {
        let claude = CapabilityReport::for_adapter("claude");
        let codex = CapabilityReport::for_adapter("codex");
        assert_eq!(
            claude
                .statuses
                .iter()
                .map(|s| s.capability)
                .collect::<Vec<_>>(),
            codex
                .statuses
                .iter()
                .map(|s| s.capability)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn policy_can_narrow_but_not_promote_unsupported_adapter_support() {
        let denied = CapabilityReport::for_adapter("claude").with_policy(|cap| match cap {
            CapabilityId::RepoWrite => PolicyDecision::Deny,
            _ => PolicyDecision::Allow,
        });
        assert_eq!(
            denied.support(CapabilityId::RepoWrite),
            SupportLevel::Unsupported
        );

        let unknown =
            CapabilityReport::for_adapter("future").with_policy(|_| PolicyDecision::Allow);
        assert_eq!(
            unknown.support(CapabilityId::RepoRead),
            SupportLevel::Unsupported
        );
    }

    #[test]
    fn every_integration_id_round_trips_through_its_wire_name() {
        for integration in IntegrationId::ALL {
            assert_eq!(
                IntegrationId::parse(integration.as_str()),
                Some(integration)
            );
            assert_eq!(
                serde_json::to_value(integration).expect("encode"),
                serde_json::Value::String(integration.as_str().to_string()),
                "the serde rename and as_str must agree for {integration}"
            );
        }
        assert_eq!(IntegrationId::parse("browser.open"), None);
    }

    #[test]
    fn a_report_distinguishes_available_unavailable_and_unverified_integrations() {
        let report = CapabilityReport::for_adapter("claude").with_integrations(vec![
            IntegrationStatus::available(IntegrationId::ArtifactRender, "zirv renderer"),
            IntegrationStatus::unverified(IntegrationId::Mcp, "1 server", "not contacted"),
            IntegrationStatus::unavailable(
                IntegrationId::Browser,
                "no browser",
                "install chromium",
            ),
        ]);
        assert_eq!(
            report.integration(IntegrationId::ArtifactRender),
            IntegrationState::Available
        );
        assert_eq!(
            report.integration(IntegrationId::Mcp),
            IntegrationState::Unverified
        );
        assert_eq!(
            report.integration(IntegrationId::Browser),
            IntegrationState::Unavailable
        );
        // A never-discovered integration is unavailable, not permitted.
        assert_eq!(
            report.integration(IntegrationId::WebSearch),
            IntegrationState::Unavailable
        );
    }

    #[test]
    fn workflow_admission_refuses_an_unavailable_integration_and_names_the_missing_piece() {
        let report = CapabilityReport::for_adapter("claude").with_integrations(vec![
            IntegrationStatus::unavailable(
                IntegrationId::Browser,
                "no Chromium-family browser was discovered",
                "install chromium/google-chrome/microsoft-edge",
            ),
            IntegrationStatus::unverified(IntegrationId::Mcp, "1 server", "not contacted"),
        ]);
        let refusal = report
            .admit(&[IntegrationId::Browser])
            .expect_err("an unavailable integration must not admit a step");
        assert!(refusal.contains("browser"), "{refusal}");
        assert!(refusal.contains("install chromium"), "{refusal}");
        assert!(
            report.admit(&[IntegrationId::Mcp]).is_ok(),
            "unverified is not the same as unavailable"
        );
    }

    #[test]
    fn canonical_policy_maps_to_provider_neutral_prerequisites() {
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            repo_fs_write: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let report = CapabilityReport::for_policy("claude", &policy);
        assert_eq!(
            report.support(CapabilityId::ShellExec),
            SupportLevel::Unsupported
        );
        assert_eq!(
            report.support(CapabilityId::TestRun),
            SupportLevel::Unsupported
        );
        assert_eq!(
            report.support(CapabilityId::GitWorktree),
            SupportLevel::Unsupported
        );
        assert_eq!(
            report.support(CapabilityId::RepoWrite),
            SupportLevel::OperatorControlled
        );
        assert_eq!(
            report.support(CapabilityId::RepoRead),
            SupportLevel::OperatorControlled
        );
    }
}
