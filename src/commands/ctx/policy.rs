//! zirv's own canonical permissions policy, and the honest translation of it
//! onto whatever the harness in front of it can actually enforce.
//!
//! Adapters describe verified launch mechanisms; Markdown instructions remain advisory.
//! Reports distinguish enforced, degraded, unsupported and operator-controlled stances.
//!
//! Repo layers may only narrow policy; operator environment overrides win outright (#43).
//! ```text
//! final(capability) = env(capability)                       if set
//!                   else max(home(capability), repo(capability))
//! ```
//! `Stance` orders `Allow < Ask < Deny`, making `max` narrowing by construction.
//! Network preserves absence separately; host allowlists reject repo widening (#727).
//!
//! Resolution and evaluation are pure, like `rot.rs`: no clock, no filesystem, no process
//! env reads -- the env layer arrives as a caller-owned `EnvLookup` closure instead.

// Canonical repo text is advisory; only verified adapter mechanisms may claim enforcement (#41, #43, #44, #46).
#![allow(dead_code)]

use super::CtxResult;
use super::adapters::AgentAdapter;
use super::config::EnvLookup;
use serde::{Deserialize, Serialize};

/// Harness-neutral permissions, independent of any adapter's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    RepoFsWrite,
    OutsideRepoFsWrite,
    ShellExec,
    Network,
    /// Unattended authority: `Allow` permits bypassing approval, `Ask` requires it,
    /// `Deny` prohibits operations needing approval.
    Approval,
    /// Includes history rewrites and destructive operations, not only pushes.
    GitPushDestructive,
    /// MCP servers share the harness tool gate.
    ToolAccess,
}

impl Capability {
    /// One ordered capability list keeps every report's coverage and ordering consistent.
    pub const ALL: [Capability; 7] = [
        Capability::RepoFsWrite,
        Capability::OutsideRepoFsWrite,
        Capability::ShellExec,
        Capability::Network,
        Capability::Approval,
        Capability::GitPushDestructive,
        Capability::ToolAccess,
    ];

    /// The `[policy]` key that sets this capability's stance.
    pub fn key(self) -> &'static str {
        match self {
            Capability::RepoFsWrite => "repo_fs_write",
            Capability::OutsideRepoFsWrite => "outside_repo_fs_write",
            Capability::ShellExec => "shell_exec",
            Capability::Network => "network",
            Capability::Approval => "approval",
            Capability::GitPushDestructive => "git_push_destructive",
            Capability::ToolAccess => "tool_access",
        }
    }

    /// Operator override applied by [`resolve`], outside the shared config merge.
    pub fn env_var(self) -> &'static str {
        match self {
            Capability::RepoFsWrite => "ZIRV_CTX_POLICY_REPO_FS_WRITE",
            Capability::OutsideRepoFsWrite => "ZIRV_CTX_POLICY_OUTSIDE_REPO_FS_WRITE",
            Capability::ShellExec => "ZIRV_CTX_POLICY_SHELL_EXEC",
            Capability::Network => "ZIRV_CTX_POLICY_NETWORK",
            Capability::Approval => "ZIRV_CTX_POLICY_APPROVAL",
            Capability::GitPushDestructive => "ZIRV_CTX_POLICY_GIT_PUSH_DESTRUCTIVE",
            Capability::ToolAccess => "ZIRV_CTX_POLICY_TOOL_ACCESS",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::RepoFsWrite => "repository filesystem writes",
            Capability::OutsideRepoFsWrite => "writes outside the repository",
            Capability::ShellExec => "shell execution",
            Capability::Network => "network access",
            Capability::Approval => "approval/ask behavior",
            Capability::GitPushDestructive => "git push / destructive git",
            Capability::ToolAccess => "MCP/tool access",
        }
    }
}

/// Ordered `Allow < Ask < Deny` so `max` can only narrow permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    /// Imposes no zirv restriction; the harness and operator settings govern.
    #[default]
    Allow,
    /// Approval must come explicitly from the operator.
    Ask,
    Deny,
}

impl Stance {
    pub fn label(self) -> &'static str {
        match self {
            Stance::Allow => "allow",
            Stance::Ask => "ask",
            Stance::Deny => "deny",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "allow" => Some(Stance::Allow),
            "ask" => Some(Stance::Ask),
            "deny" => Some(Stance::Deny),
            _ => None,
        }
    }
}

/// Policy and the native broker share destination identity so their scope checks cannot diverge (#727).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "RawNetworkTarget")]
pub struct NetworkTarget {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

/// Persisted targets must pass the constructor's scheme/host validation too.
/// Normalize case and trailing dots so aliases cannot disagree in subset checks (#727).
#[derive(Deserialize)]
struct RawNetworkTarget {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl TryFrom<RawNetworkTarget> for NetworkTarget {
    type Error = String;

    fn try_from(raw: RawNetworkTarget) -> Result<Self, Self::Error> {
        NetworkTarget::new(&raw.scheme, &raw.host, raw.port)
            .map_err(|error| format!("{}://{}: {error}", raw.scheme, raw.host))
    }
}

/// Ordinary capabilities default to `Allow` because an absent key must contribute no restriction.
/// Unknown keys fail parsing so typos cannot silently widen permissions (#727).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectivePolicy {
    pub repo_fs_write: Stance,
    pub outside_repo_fs_write: Stance,
    pub shell_exec: Stance,
    /// Absence must neither open Codex's closed native network default nor claim a zirv-imposed denial.
    /// Keep `None` distinct from every explicit stance so evaluation can omit the unconfigured row.
    pub network: Option<Stance>,
    /// Non-empty lists scope `network`; empty means no additional scope.
    /// Home/repo resolution is narrow-only, with no environment override (#727).
    pub network_allowlist: Vec<NetworkTarget>,
    pub approval: Stance,
    pub git_push_destructive: Stance,
    pub tool_access: Stance,
}

impl EffectivePolicy {
    /// Missing network policy answers conservatively as `Deny`; reporters must inspect `.network`
    /// directly so absence never becomes a claim that zirv explicitly denied network.
    pub fn stance(&self, capability: Capability) -> Stance {
        match capability {
            Capability::RepoFsWrite => self.repo_fs_write,
            Capability::OutsideRepoFsWrite => self.outside_repo_fs_write,
            Capability::ShellExec => self.shell_exec,
            Capability::Network => self.network.unwrap_or(Stance::Deny),
            Capability::Approval => self.approval,
            Capability::GitPushDestructive => self.git_push_destructive,
            Capability::ToolAccess => self.tool_access,
        }
    }

    /// Callers must handle `Network` separately because its optional stance preserves absence.
    fn stance_mut(&mut self, capability: Capability) -> &mut Stance {
        match capability {
            Capability::RepoFsWrite => &mut self.repo_fs_write,
            Capability::OutsideRepoFsWrite => &mut self.outside_repo_fs_write,
            Capability::ShellExec => &mut self.shell_exec,
            Capability::Network => {
                unreachable!(
                    "network has no plain Stance slot (Option<Stance>); callers must skip \
                     Capability::Network and assign .network directly -- see resolve_network"
                )
            }
            Capability::Approval => &mut self.approval,
            Capability::GitPushDestructive => &mut self.git_push_destructive,
            Capability::ToolAccess => &mut self.tool_access,
        }
    }

    /// Narrows ordinary stances by `max`; [`resolve`] must separately fold network and its allowlist.
    /// `max` can never return the smaller of two stances, so this privilege-widening defense is a
    /// property of `Stance`'s ordering, not a check a future edit could drop.
    pub fn narrowed_by(self, narrower: EffectivePolicy) -> EffectivePolicy {
        let mut out = self.clone();
        for capability in Capability::ALL {
            if capability == Capability::Network {
                continue;
            }
            let stance = self.stance(capability).max(narrower.stance(capability));
            *out.stance_mut(capability) = stance;
        }
        out
    }

    /// Config-load failure denies every capability; an unreadable policy must never become permissive (#44).
    pub fn fail_closed() -> Self {
        EffectivePolicy {
            repo_fs_write: Stance::Deny,
            outside_repo_fs_write: Stance::Deny,
            shell_exec: Stance::Deny,
            network: Some(Stance::Deny),
            network_allowlist: Vec::new(),
            approval: Stance::Deny,
            git_push_destructive: Stance::Deny,
            tool_access: Stance::Deny,
        }
    }

    /// This is a report-only baseline: folding it into policy would restrict headless launches
    /// and make some operator `Allow` overrides impossible under the narrowing `max`.
    pub fn interactive_baseline() -> Self {
        EffectivePolicy {
            // `Edit(./**)` is pre-approved on the allow list.
            repo_fs_write: Stance::Allow,
            // `--permission-mode default` prompts for writes outside the pre-approved repo.
            outside_repo_fs_write: Stance::Ask,
            // `safety.rs` is the prompting gate: allowed and unclassified commands run silently;
            // ask rules prompt and deny rules refuse.
            shell_exec: Stance::Ask,
            // `WebFetch`/`WebSearch` are pre-approved.
            network: Some(Stance::Allow),
            // Host scoping requires operator opt-in (#727).
            network_allowlist: Vec::new(),
            approval: Stance::Ask,
            // Force-push and history rewrites are in the built-in ask set.
            git_push_destructive: Stance::Ask,
            tool_access: Stance::Allow,
        }
    }
}

/// Resolves home/repo narrowing plus operator env overrides; malformed stances hard-error.
/// Network preserves absent keys; host allowlists reject repo widening and have no env override (#727).
pub fn resolve(
    home: Option<toml::Value>,
    repo: Option<toml::Value>,
    env: EnvLookup<'_>,
) -> CtxResult<EffectivePolicy> {
    let home_network = parse_network_layer(&home, "~/.zirv/ctx.toml")?;
    let repo_network = parse_network_layer(&repo, "<repo>/.zirv/ctx.toml")?;
    let home_allowlist = parse_network_allowlist_layer(&home, "~/.zirv/ctx.toml")?;
    let repo_allowlist = parse_network_allowlist_layer(&repo, "<repo>/.zirv/ctx.toml")?;

    let mut resolved = parse_layer(home, "~/.zirv/ctx.toml")?
        .narrowed_by(parse_layer(repo, "<repo>/.zirv/ctx.toml")?);
    resolved.network = resolve_network(home_network, repo_network);
    resolved.network_allowlist =
        resolve_network_allowlist(home_allowlist.unwrap_or_default(), repo_allowlist)?;

    for capability in Capability::ALL {
        if capability == Capability::Network {
            continue;
        }
        let Some(raw) = env(capability.env_var()) else {
            continue;
        };
        let Some(stance) = Stance::parse(&raw) else {
            return Err(format!(
                "{}: expected allow, ask or deny, got '{raw}'",
                capability.env_var()
            )
            .into());
        };
        *resolved.stance_mut(capability) = stance;
    }

    if let Some(raw) = env(Capability::Network.env_var()) {
        let Some(stance) = Stance::parse(&raw) else {
            return Err(format!(
                "{}: expected allow, ask or deny, got '{raw}'",
                Capability::Network.env_var()
            )
            .into());
        };
        resolved.network = Some(stance);
    }

    Ok(resolved)
}

fn parse_layer(layer: Option<toml::Value>, origin: &str) -> CtxResult<EffectivePolicy> {
    let Some(layer) = layer else {
        return Ok(EffectivePolicy::default());
    };
    layer
        .try_into()
        .map_err(|e| format!("{origin}: invalid [policy] section: {e}").into())
}

/// Preserves an absent `network` key so a silent repo cannot override the home stance.
fn parse_network_layer(layer: &Option<toml::Value>, origin: &str) -> CtxResult<Option<Stance>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct NetworkOnly {
        network: Option<Stance>,
    }
    let Some(layer) = layer else {
        return Ok(None);
    };
    let parsed: NetworkOnly = layer
        .clone()
        .try_into()
        .map_err(|e| format!("{origin}: invalid [policy] section: {e}"))?;
    Ok(parsed.network)
}

/// Both silent means `None`; otherwise home silence is `Deny` because the operator never opted in.
/// Repo silence is neutral (`Allow`), but explicit `Ask` must still narrow home `Allow`.
fn resolve_network(home: Option<Stance>, repo: Option<Stance>) -> Option<Stance> {
    if home.is_none() && repo.is_none() {
        return None;
    }
    Some(std::cmp::max(
        home.unwrap_or(Stance::Deny),
        repo.unwrap_or(Stance::Allow),
    ))
}

/// Preserve absence separately: a silent repo must not erase the operator's allowlist.
fn parse_network_allowlist_layer(
    layer: &Option<toml::Value>,
    origin: &str,
) -> CtxResult<Option<Vec<NetworkTarget>>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct AllowlistOnly {
        network_allowlist: Option<Vec<NetworkTarget>>,
    }
    let Some(layer) = layer else {
        return Ok(None);
    };
    let parsed: AllowlistOnly = layer
        .clone()
        .try_into()
        .map_err(|e| format!("{origin}: invalid [policy] section: {e}"))?;
    Ok(parsed.network_allowlist)
}

/// A silent repo keeps home's allowlist; an explicit list must be a subset, including empty.
/// Ungranted destinations hard-error so repo overreach cannot disappear in an intersection (#727).
fn resolve_network_allowlist(
    home: Vec<NetworkTarget>,
    repo: Option<Vec<NetworkTarget>>,
) -> CtxResult<Vec<NetworkTarget>> {
    let Some(repo) = repo else {
        return Ok(home);
    };
    if let Some(ungranted) = repo.iter().find(|target| !home.contains(target)) {
        let port = ungranted.port.map(|p| format!(":{p}")).unwrap_or_default();
        return Err(format!(
            "<repo>/.zirv/ctx.toml: [policy] network_allowlist may only narrow the operator's \
             own list; {}://{}{port} is not in it",
            ungranted.scheme, ungranted.host,
        )
        .into());
    }
    Ok(repo)
}

/// Enforcement claims require verified mechanisms; prompt-only support is [`Support::Unsupported`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// A verified per-run mechanism enforces exactly the requested stance.
    Enforced,
    /// A verified mechanism only approximates the stance; the report must state the difference.
    Degraded,
    /// No verified per-run mechanism; advisory prompt text is not enforcement.
    Unsupported,
    /// Operator harness settings are read, never rewritten; `Allow` likewise imposes no zirv restriction.
    OperatorControlled,
}

impl Support {
    /// Labels must distinguish advisory text from enforcement even out of context.
    pub fn label(self) -> &'static str {
        match self {
            Support::Enforced => "enforced",
            Support::Degraded => "degraded (partially enforced)",
            Support::Unsupported => "not enforced (advisory only)",
            Support::OperatorControlled => "operator-controlled",
        }
    }

    /// Only exact enforcement qualifies; degraded mechanisms must remain visible as gaps.
    pub fn is_fully_enforced(self) -> bool {
        matches!(self, Support::Enforced)
    }
}

/// Enforcement claims must name a verified mechanism so advisory prose cannot masquerade as a guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub support: Support,
    /// Operator-facing mechanism name, or the reason exact enforcement is unavailable.
    pub mechanism: &'static str,
}

impl CapabilityDescriptor {
    pub fn enforced(mechanism: &'static str) -> Self {
        Self {
            support: Support::Enforced,
            mechanism,
        }
    }

    pub fn degraded(mechanism: &'static str) -> Self {
        Self {
            support: Support::Degraded,
            mechanism,
        }
    }

    pub fn operator_controlled(mechanism: &'static str) -> Self {
        Self {
            support: Support::OperatorControlled,
            mechanism,
        }
    }

    /// A checked-but-unsuitable mechanism needs its specific reason reported, not the generic advisory fallback.
    pub fn unsupported(mechanism: &'static str) -> Self {
        Self {
            support: Support::Unsupported,
            mechanism,
        }
    }

    /// Default for unverified harness/capability pairs: prompt text supplies no enforcement guarantee.
    pub fn advisory_only() -> Self {
        Self::unsupported(
            "no verified per-run mechanism; prompt text is advisory context, not enforcement",
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityOutcome {
    pub capability: Capability,
    pub stance: Stance,
    pub support: Support,
    pub mechanism: &'static str,
}

/// Structured gaps reach the delegator even when the worker never reports them (#230).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CapabilityWarning {
    /// [`Capability::label`] -- human-readable, not the `[policy]` key.
    pub capability: String,
    pub mechanism: String,
    /// Requested stance and delivered support, e.g. `"deny -- degraded (partially enforced)"`.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReport {
    pub adapter: &'static str,
    /// Disambiguates interactive approval prompts from headless fail-closed refusals.
    pub mode: super::adapters::LaunchMode,
    pub outcomes: Vec<CapabilityOutcome>,
}

impl PolicyReport {
    /// Every non-exact outcome must remain visible; even degraded and operator-controlled stances lack zirv's guarantee.
    pub fn unenforced(&self) -> Vec<&CapabilityOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| {
                outcome.stance != Stance::Allow && !outcome.support.is_fully_enforced()
            })
            .collect()
    }

    /// Distinguish partial enforcement from mere advice: a degraded mechanism still imposes real restrictions.
    pub fn partially_enforced(&self) -> Vec<&CapabilityOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.support == Support::Degraded)
            .collect()
    }

    /// Warn delegators when a zirv enforcement promise is degraded or unsupported (#230).
    /// Operator-owned settings and unconstrained `Allow` are not failed zirv enforcement promises.
    pub fn degraded_capabilities(&self) -> Vec<CapabilityWarning> {
        self.outcomes
            .iter()
            .filter(|outcome| matches!(outcome.support, Support::Degraded | Support::Unsupported))
            .map(|outcome| CapabilityWarning {
                capability: outcome.capability.label().to_string(),
                mechanism: outcome.mechanism.to_string(),
                detail: format!("{} -- {}", outcome.stance.label(), outcome.support.label()),
            })
            .collect()
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "policy on {} ({} launch):\n",
            self.adapter,
            self.mode.label()
        );
        for outcome in &self.outcomes {
            out.push_str(&format!(
                "  {}: {} -- {} ({})\n",
                outcome.capability.label(),
                outcome.stance.label(),
                outcome.support.label(),
                outcome.mechanism
            ));
        }
        // Headless reports must not claim an interactive baseline; their rows describe deny-by-omission.
        if self.mode.is_interactive() {
            out.push_str("  shipped interactive baseline (before any [policy] table):\n");
            let baseline = EffectivePolicy::interactive_baseline();
            for capability in Capability::ALL {
                out.push_str(&format!(
                    "    {}: {}\n",
                    capability.label(),
                    baseline.stance(capability).label()
                ));
            }
        }
        out
    }
}

/// Descriptor evaluation must stay free of clock, filesystem and environment reads.
/// `Allow` imposes nothing; omit absent network so the report never claims an unconfigured denial.
pub fn evaluate(
    policy: &EffectivePolicy,
    adapter: &dyn AgentAdapter,
    mode: super::adapters::LaunchMode,
) -> PolicyReport {
    let outcomes = Capability::ALL
        .into_iter()
        .filter_map(|capability| {
            let stance = if capability == Capability::Network {
                policy.network?
            } else {
                policy.stance(capability)
            };
            // A non-empty allowlist needs its descriptor before the `Allow` catch-all;
            // `Deny` already blocks all network and needs no host scoping (#727).
            let descriptor = if capability == Capability::Network
                && stance != Stance::Deny
                && !policy.network_allowlist.is_empty()
            {
                adapter.network_allowlist_support(&policy.network_allowlist, stance, mode)
            } else {
                match stance {
                    Stance::Allow => CapabilityDescriptor::operator_controlled(
                        "zirv declares no restriction; the harness's own defaults and the \
                         operator's own settings decide",
                    ),
                    _ => adapter.policy_support(capability, stance, mode),
                }
            };
            Some(CapabilityOutcome {
                capability,
                stance,
                support: descriptor.support,
                mechanism: descriptor.mechanism,
            })
        })
        .collect();
    PolicyReport {
        adapter: adapter.name(),
        mode,
        outcomes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn table(toml_text: &str) -> Option<toml::Value> {
        Some(toml::from_str::<toml::Value>(toml_text).expect("test toml parses"))
    }

    /// The spec's own interactive defaults table, stated once so a report can
    /// show an operator what an unconfigured interactive launch carries.
    #[test]
    fn the_interactive_baseline_is_the_specs_own_defaults_table() {
        let baseline = EffectivePolicy::interactive_baseline();
        assert_eq!(baseline.repo_fs_write, Stance::Allow);
        assert_eq!(baseline.outside_repo_fs_write, Stance::Ask);
        assert_eq!(baseline.network, Some(Stance::Allow));
        assert_eq!(baseline.shell_exec, Stance::Ask);
        assert_eq!(baseline.approval, Stance::Ask);
        assert_eq!(baseline.git_push_destructive, Stance::Ask);
        assert_eq!(baseline.tool_access, Stance::Allow);
    }

    /// SECURITY: the baseline is a REPORTED fact, never a fold input.
    /// `EffectivePolicy::default()` must stay all-`Allow` except `network`
    /// (its own documented exception, `EffectivePolicy`'s `Default` impl) --
    /// the rest is what `narrowed_by`'s widening defense and `resolve`'s fold
    /// rest on, and what makes `ZIRV_CTX_POLICY_*` able to loosen at all.
    #[test]
    fn the_interactive_baseline_does_not_touch_the_default_or_the_fold() {
        assert_ne!(
            EffectivePolicy::interactive_baseline(),
            EffectivePolicy::default()
        );
        for capability in Capability::ALL {
            let expected = if capability == Capability::Network {
                Stance::Deny
            } else {
                Stance::Allow
            };
            assert_eq!(
                EffectivePolicy::default().stance(capability),
                expected,
                "{} must still default to {}",
                capability.key(),
                expected.label()
            );
        }
    }

    /// The bug this round fixes (2026-08-26): before `network` became
    /// `Option<Stance>`, `EffectivePolicy::default()`'s own `network:
    /// Stance::Deny` meant `evaluate` always produced a `Network` outcome on
    /// an unconfigured install, and its rendered line read "network access:
    /// deny -- ..." -- implying zirv itself was denying network, when in
    /// truth no operator layer had ever named it (codex's own native
    /// default already denies it, with no zirv-added flag at all). A report
    /// built from a wholly-default policy must contain no `Network` row and
    /// no "network" text at all.
    #[test]
    fn a_default_policys_report_carries_no_spurious_network_row() {
        let policy = EffectivePolicy::default();
        let claude = ClaudeAdapter::new(None);
        let codex = CodexAdapter::new(None);
        for adapter in [&claude as &dyn AgentAdapter, &codex as &dyn AgentAdapter] {
            let report = evaluate(&policy, adapter, adapters::LaunchMode::Headless);
            assert!(
                !report
                    .outcomes
                    .iter()
                    .any(|outcome| outcome.capability == Capability::Network),
                "{}: a default (unconfigured) policy must render no Network row at all",
                report.adapter
            );
            assert!(
                !report.render().contains("network"),
                "{}: the rendered report must not mention network at all when unconfigured",
                report.adapter
            );
        }
    }

    /// Once an operator layer explicitly names `network` (whatever the
    /// stance), the row comes back -- `evaluate` only omits it for `None`,
    /// never for a `Some` the operator chose, even `Some(Stance::Deny)`.
    #[test]
    fn an_explicitly_configured_network_stance_still_renders_its_row() {
        let policy = EffectivePolicy {
            network: Some(Stance::Deny),
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        assert!(
            report
                .outcomes
                .iter()
                .any(|outcome| outcome.capability == Capability::Network),
            "an operator-chosen network stance must still be reported"
        );
    }

    /// A report says which posture it describes, and an interactive one shows
    /// the shipped baseline underneath the per-capability lines.
    #[test]
    fn a_rendered_report_names_the_launch_mode_and_the_interactive_baseline() {
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);

        let interactive = evaluate(&policy, &claude, adapters::LaunchMode::Interactive).render();
        assert!(interactive.starts_with("policy on claude (interactive launch):"));
        assert!(interactive.contains("shipped interactive baseline"));
        assert!(interactive.contains("writes outside the repository: ask"));

        let headless = evaluate(&policy, &claude, adapters::LaunchMode::Headless).render();
        assert!(headless.starts_with("policy on claude (headless launch):"));
        assert!(
            !headless.contains("shipped interactive baseline"),
            "a headless report must not advertise the interactive baseline"
        );
    }

    /// The honesty half of the posture split: on an INTERACTIVE claude launch
    /// zirv really does pin a mechanism for an `Ask` stance now
    /// (`--permission-mode default` plus the safety hook as sole gate), so
    /// those cells stop being `OperatorControlled`. Headless is unchanged --
    /// under `dontAsk` a hook `ask` is suppressed, so there is nothing to
    /// claim.
    #[test]
    fn claude_claims_an_ask_mechanism_only_on_an_interactive_launch() {
        let claude = ClaudeAdapter::new(None);
        for capability in [
            Capability::ShellExec,
            Capability::Approval,
            Capability::OutsideRepoFsWrite,
        ] {
            let interactive =
                claude.policy_support(capability, Stance::Ask, adapters::LaunchMode::Interactive);
            assert_eq!(
                interactive.support,
                Support::Degraded,
                "{} must report a real, partial ask mechanism interactively",
                capability.key()
            );
            let headless =
                claude.policy_support(capability, Stance::Ask, adapters::LaunchMode::Headless);
            assert_eq!(
                headless.support,
                Support::OperatorControlled,
                "{} must claim nothing headlessly",
                capability.key()
            );
        }
        // Never `Enforced`: the hook is registered for the Bash tool only.
        assert_ne!(
            claude
                .policy_support(
                    Capability::ToolAccess,
                    Stance::Ask,
                    adapters::LaunchMode::Interactive,
                )
                .support,
            Support::Enforced
        );
    }

    /// Codex's own honest answer for the same question. The mechanism string
    /// must say what codex's approval actually is -- a SANDBOX-boundary
    /// escalation, whose granularity is codex's own -- and must state that
    /// zirv's per-command classification is not carried onto this harness at
    /// all. Anything vaguer reads as parity with claude, which is the
    /// over-claim `policy.rs` exists to prevent.
    #[test]
    fn codex_reports_its_interactive_ask_posture_as_degraded_and_names_the_gap() {
        let codex = CodexAdapter::new(None).with_on_request_approval_forced(true);
        let descriptor = codex.policy_support(
            Capability::Approval,
            Stance::Ask,
            adapters::LaunchMode::Interactive,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert!(descriptor.mechanism.contains("on-request"));
        assert!(
            descriptor.mechanism.contains("sandbox"),
            "the report must say the sandbox is what contains damage: {}",
            descriptor.mechanism
        );
        assert!(
            descriptor.mechanism.contains("per-command"),
            "the report must name what codex cannot match: {}",
            descriptor.mechanism
        );

        let unsure = CodexAdapter::new(None).with_on_request_approval_forced(false);
        assert_eq!(
            unsure
                .policy_support(
                    Capability::Approval,
                    Stance::Ask,
                    adapters::LaunchMode::Interactive,
                )
                .support,
            Support::OperatorControlled,
            "an install that cannot take `on-request` must claim nothing"
        );
    }

    /// Issue #230 item 3: `degraded_capabilities` keeps only `Degraded`/
    /// `Unsupported` outcomes, dropping `Enforced` (nothing to warn about)
    /// and `OperatorControlled` (either `Allow`, or a capability only the
    /// operator's own harness settings govern) -- exercised on a real
    /// mixed report rather than a hand-built one, so this pins the actual
    /// adapter behaviour rather than a synthetic fixture.
    #[test]
    fn degraded_capabilities_keeps_only_degraded_and_unsupported_outcomes() {
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny, // claude: Degraded (SANDBOX-adjacent) on codex; Enforced on claude
            shell_exec: Stance::Deny,    // codex: Unsupported
            approval: Stance::Ask,       // Allow's stance is never reached here since Ask
            git_push_destructive: Stance::Allow, // OperatorControlled: Allow, nothing to warn about
            ..EffectivePolicy::default()
        };
        let codex = CodexAdapter::new(None);
        let report = evaluate(&policy, &codex, adapters::LaunchMode::Headless);

        let warnings = report.degraded_capabilities();
        assert!(
            !warnings.is_empty(),
            "this policy must produce at least one real warning on codex"
        );
        for warning in &warnings {
            assert!(!warning.capability.is_empty());
            assert!(!warning.mechanism.is_empty());
            assert!(!warning.detail.is_empty());
        }
        // Every outcome this report actually produced with a Degraded or
        // Unsupported support must show up, one-for-one, as a warning; every
        // Enforced/OperatorControlled outcome must not.
        let expected: Vec<_> = report
            .outcomes
            .iter()
            .filter(|o| matches!(o.support, Support::Degraded | Support::Unsupported))
            .map(|o| o.capability.label().to_string())
            .collect();
        let got: Vec<_> = warnings.iter().map(|w| w.capability.clone()).collect();
        assert_eq!(got, expected);
        assert!(
            report
                .outcomes
                .iter()
                .any(|o| o.support == Support::OperatorControlled),
            "the fixture must actually exercise an OperatorControlled outcome for this test to \
             mean anything"
        );
        for outcome in report
            .outcomes
            .iter()
            .filter(|o| o.support == Support::OperatorControlled)
        {
            assert!(
                !got.contains(&outcome.capability.label().to_string()),
                "OperatorControlled must never appear as a degraded-capability warning"
            );
        }
    }

    /// A report with nothing but `Enforced`/`OperatorControlled` outcomes
    /// (the shipped default, `Allow` everywhere) warns about nothing.
    #[test]
    fn degraded_capabilities_is_empty_for_a_default_policy() {
        let policy = EffectivePolicy::default();
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        assert!(report.degraded_capabilities().is_empty());
    }

    #[test]
    fn stances_are_ordered_least_to_most_restrictive() {
        assert!(Stance::Allow < Stance::Ask);
        assert!(Stance::Ask < Stance::Deny);
        assert_eq!(Stance::default(), Stance::Allow);
    }

    #[test]
    fn fail_closed_denies_every_capability_and_differs_from_default() {
        let closed = EffectivePolicy::fail_closed();
        for capability in Capability::ALL {
            assert_eq!(
                closed.stance(capability),
                Stance::Deny,
                "{} should be denied by the fail-closed fallback",
                capability.key()
            );
        }
        assert_ne!(closed, EffectivePolicy::default());
    }

    /// `network` is the one deliberate exception (see `EffectivePolicy`'s own
    /// doc comment): every other capability's default is `Allow`, "zirv
    /// declares no restriction of its own"; `network`'s default is `None`,
    /// "no operator layer has ever named it" -- distinct from `Some(Deny)`,
    /// which would claim zirv itself denies network on an unconfigured
    /// install.
    #[test]
    fn a_default_policy_declares_no_restriction_at_all_except_network() {
        let policy = EffectivePolicy::default();
        for capability in Capability::ALL {
            if capability == Capability::Network {
                continue;
            }
            assert_eq!(
                policy.stance(capability),
                Stance::Allow,
                "{} should default to allow",
                capability.key()
            );
        }
        assert_eq!(
            policy.network, None,
            "network should default to None -- no operator layer has ever named it, matching \
             what an unwired install has always done without claiming zirv denies it"
        );
    }

    /// The privilege-widening defense, stated directly on the fold: whatever
    /// an untrusted layer says, the result is never looser than the operator's
    /// own stance. Deliberately an explicit all-`Allow` literal, not
    /// `EffectivePolicy::default()`: `default()` is not the uniformly loosest
    /// possible value (`network` defaults to `None`, outside the fold this
    /// test exercises), so it would not represent "an attempt to widen every
    /// capability to `Allow`". `network` is left at its own default (`None`)
    /// on both sides here rather than given a value: it is deliberately
    /// excluded from `narrowed_by`'s generic loop (see that method's own doc
    /// comment) and has its own narrowing tests through `resolve`/
    /// `resolve_network` below, so a value here would only assert that
    /// `narrowed_by` leaves it untouched, not that anything narrows.
    #[test]
    fn narrowing_never_loosens_any_capability() {
        let operator = EffectivePolicy {
            repo_fs_write: Stance::Ask,
            outside_repo_fs_write: Stance::Deny,
            shell_exec: Stance::Deny,
            network: None,
            network_allowlist: Vec::new(),
            approval: Stance::Ask,
            git_push_destructive: Stance::Deny,
            tool_access: Stance::Ask,
        };
        let widening_attempt = EffectivePolicy {
            repo_fs_write: Stance::Allow,
            outside_repo_fs_write: Stance::Allow,
            shell_exec: Stance::Allow,
            network: None,
            network_allowlist: Vec::new(),
            approval: Stance::Allow,
            git_push_destructive: Stance::Allow,
            tool_access: Stance::Allow,
        };
        assert_eq!(operator.clone().narrowed_by(widening_attempt), operator);
    }

    /// `network` is deliberately left at its own default (`None`) on both
    /// sides: `narrowed_by` excludes it from the generic fold entirely (see
    /// that method's own doc comment), so this test's job is the other six
    /// capabilities. `network`'s own narrowing is exercised through
    /// `resolve`/`resolve_network` in the tests below instead.
    #[test]
    fn narrowing_takes_the_stricter_of_the_two_per_capability() {
        let operator = EffectivePolicy {
            shell_exec: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let repo = EffectivePolicy {
            shell_exec: Stance::Deny,
            repo_fs_write: Stance::Ask,
            ..EffectivePolicy::default()
        };
        let narrowed = operator.narrowed_by(repo);
        assert_eq!(narrowed.shell_exec, Stance::Deny);
        assert_eq!(narrowed.repo_fs_write, Stance::Ask);
    }

    /// SECURITY: the cloned-repository privilege-widening case, exercised
    /// through `resolve` rather than the fold helper -- a repo `[policy]`
    /// table naming the loosest stance for every capability must not move a
    /// single one of the operator's own.
    #[test]
    fn a_repo_policy_table_cannot_widen_any_operator_stance() {
        let home = table(
            "[policy]\nrepo_fs_write = \"ask\"\noutside_repo_fs_write = \"deny\"\nshell_exec = \
             \"deny\"\nnetwork = \"deny\"\napproval = \"ask\"\ngit_push_destructive = \
             \"deny\"\ntool_access = \"ask\"\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let repo = table(
            "[policy]\nrepo_fs_write = \"allow\"\noutside_repo_fs_write = \
             \"allow\"\nshell_exec = \"allow\"\nnetwork = \"allow\"\napproval = \
             \"allow\"\ngit_push_destructive = \"allow\"\ntool_access = \"allow\"\n",
        )
        .and_then(|v| v.get("policy").cloned());

        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.repo_fs_write, Stance::Ask);
        assert_eq!(resolved.outside_repo_fs_write, Stance::Deny);
        assert_eq!(resolved.shell_exec, Stance::Deny);
        assert_eq!(resolved.network, Some(Stance::Deny));
        assert_eq!(resolved.approval, Stance::Ask);
        assert_eq!(resolved.git_push_destructive, Stance::Deny);
        assert_eq!(resolved.tool_access, Stance::Ask);
    }

    /// The other half of "may narrow, never widen": a repo tightening a stance
    /// the operator left loose is honored, because narrowing is always safe.
    /// `network` stays `None` here, unlike every other untouched capability
    /// (`repo_fs_write` etc., implicitly `Allow` via the assertions below) --
    /// but for a different reason than those six: nothing (neither home nor
    /// repo) ever names `network` at all here, which is `resolve_network`'s
    /// own "both layers silent" case, not a per-field default -- see that
    /// function's own doc comment.
    #[test]
    fn a_repo_policy_table_may_tighten_a_stance_the_operator_left_loose() {
        let repo =
            table("[policy]\nshell_exec = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.shell_exec, Stance::Deny);
        assert_eq!(resolved.repo_fs_write, Stance::Allow);
        assert_eq!(resolved.network, None);
    }

    /// Nothing anywhere ever names `network` -- `resolve_network`'s own
    /// "both layers silent" case: `None`, not a stance, since no operator
    /// layer ever expressed an opinion (see `EffectivePolicy`'s own doc
    /// comment for why `None` rather than a defaulted `Deny` matters here).
    #[test]
    fn network_resolves_to_none_when_nothing_is_configured_anywhere() {
        let vars = env_from(&[]);
        let resolved = resolve(None, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, None);
    }

    /// The actual correction this round makes (2026-08-26): home's own
    /// explicit `network = "allow"` must survive a repo whose `[policy]`
    /// table never mentions `network` at all -- the repo's silence carries
    /// no opinion of its own, so it cannot defeat the operator's. Before this
    /// round, a bare `Stance` field made "repo said nothing" and "repo
    /// explicitly denied" indistinguishable, so this used to (wrongly)
    /// resolve to `Deny`.
    #[test]
    fn network_opens_when_home_allows_it_and_the_repo_says_nothing_at_all() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Allow));
    }

    /// A repo may still always narrow: its own explicit `network = "deny"`
    /// defeats home's `allow`, exactly like every other capability.
    #[test]
    fn network_stays_denied_when_the_repo_explicitly_denies_it_even_though_home_allows_it() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let repo = table("[policy]\nnetwork = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Deny));
    }

    /// The bug this round fixes (2026-08-26): a repo's explicit `network =
    /// "ask"` must narrow a looser home stance exactly like `"deny"` does --
    /// the previous `resolve_network` only special-cased `repo == Some(Deny)`
    /// and fell through to home's own value for any other repo stance
    /// (including `Ask`), so this scenario silently resolved to `Allow`,
    /// dropping the repo's narrowing entirely. `max(home, repo)` treats every
    /// repo stance as a potential narrowing input, not just `Deny`.
    #[test]
    fn network_ask_narrows_a_looser_home_allow() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let repo = table("[policy]\nnetwork = \"ask\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(
            resolved.network,
            Some(Stance::Ask),
            "the repo's explicit ask must narrow home's allow, not be dropped in favor of it"
        );
    }

    /// The other half of "a repo may never widen, only narrow": a repo's own
    /// bare `network = "allow"`, with home silent, must NOT grant network on
    /// its own -- only the operator (home or env) can ever move `network`
    /// toward `Allow`.
    #[test]
    fn network_stays_denied_when_only_the_repo_explicitly_allows_it_and_home_says_nothing() {
        let repo = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Deny));
    }

    /// When the repo's own `[policy]` table explicitly agrees (`network =
    /// "allow"`), home's own explicit `allow` is what actually carries --
    /// repo's matching `allow` is a no-op agreement, never a grant of its
    /// own (see the "repo alone" test above, which pins that half).
    #[test]
    fn network_opens_when_home_and_repo_both_explicitly_allow_it() {
        let home = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let repo = table("[policy]\nnetwork = \"allow\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Allow));
    }

    /// The environment override sits above the fold entirely (`resolve`'s own
    /// doc comment), so it is the one path guaranteed to open `network`
    /// regardless of what any repo says -- including a repo that actively
    /// tries to deny it.
    #[test]
    fn env_can_open_network_regardless_of_what_the_repo_says() {
        let repo = table("[policy]\nnetwork = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[("ZIRV_CTX_POLICY_NETWORK", "allow")]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.network, Some(Stance::Allow));
    }

    /// Issue #727: the whole point of a repo-owned `network_allowlist` is
    /// that a checkout cannot name a destination the operator never granted.
    /// A repo layer adding a host beyond the home layer's own set is a hard
    /// error -- the same widening-rejection shape as an unparseable stance
    /// above, not a silent drop like `narrow_objective_gates`'s subset
    /// filter in `config.rs` (see `resolve_network_allowlist`'s own doc
    /// comment for why a silent drop is wrong here).
    #[test]
    fn a_repo_layer_cannot_widen_the_network_allowlist_with_a_host_home_never_granted() {
        let home = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"api.example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let repo = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"evil.example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let err = resolve(home, repo, &|k| vars.get(k).cloned())
            .expect_err("a repo may not name a host the operator never granted");
        assert!(
            err.to_string().contains("evil.example.com"),
            "the error must name the offending host: {err}"
        );
    }

    /// The other half: a repo layer may always narrow the operator's own
    /// allowlist down to a subset -- removing a host is always safe, the
    /// same direction `network` itself and every other `[policy]` capability
    /// already allow.
    #[test]
    fn a_repo_layer_may_narrow_the_network_allowlist_by_removing_a_host() {
        let home = table(
            "[policy]\nnetwork_allowlist = [\n  \
             { scheme = \"https\", host = \"api.example.com\" },\n  \
             { scheme = \"https\", host = \"docs.example.com\" },\n]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let repo = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"api.example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(
            resolved.network_allowlist,
            vec![NetworkTarget {
                scheme: "https".to_string(),
                host: "api.example.com".to_string(),
                port: None,
            }]
        );
    }

    /// A repo layer that never mentions `network_allowlist` at all takes no
    /// position, the same "silence carries no opinion" rule `network` itself
    /// follows -- the operator's own list survives untouched.
    #[test]
    fn a_repo_layer_that_never_mentions_the_allowlist_keeps_the_operators_own() {
        let home = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"api.example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, None, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(
            resolved.network_allowlist,
            vec![NetworkTarget {
                scheme: "https".to_string(),
                host: "api.example.com".to_string(),
                port: None,
            }]
        );
    }

    /// Review round (issue #727): a malformed host must hard-error at parse
    /// time, naming the offending entry -- before this fix, `NetworkTarget`'s
    /// plain derived `Deserialize` accepted any string verbatim, skipping the
    /// validation ([`NetworkTarget::new`]) every programmatic caller already
    /// goes through.
    #[test]
    fn a_malformed_host_in_the_repo_layer_hard_errors_naming_the_entry() {
        let home = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"api.example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let repo = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"evil/example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let err = resolve(home, repo, &|k| vars.get(k).cloned())
            .expect_err("a host containing '/' must not deserialize");
        assert!(
            err.to_string().contains("evil/example.com"),
            "the error must name the malformed entry: {err}"
        );
    }

    /// Review round (issue #727): routing `Deserialize` through
    /// `NetworkTarget::new` normalizes case and a trailing dot on the way in,
    /// so a repo layer spelling the operator's own granted host differently
    /// (`Example.COM.`) still resolves to the SAME entry `home` granted --
    /// narrowing is accepted rather than rejected as "a host home never
    /// granted".
    #[test]
    fn a_repo_layer_spelling_normalizes_so_narrowing_is_accepted() {
        let home = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let repo = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"HTTPS\", host = \"Example.COM.\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let resolved = resolve(home, repo, &|k| vars.get(k).cloned())
            .expect("a normalized repeat of the operator's own host must not hard-error");
        assert_eq!(
            resolved.network_allowlist,
            vec![NetworkTarget {
                scheme: "https".to_string(),
                host: "example.com".to_string(),
                port: None,
            }]
        );
    }

    /// Review round (issue #727), item 4: the operator never mentioning
    /// `network_allowlist` at all is NOT the same as granting an empty list --
    /// a repo layer naming any host at all against a silent home is naming a
    /// host the operator never granted, the same hard error as against an
    /// explicit non-empty home list.
    #[test]
    fn home_omitting_the_allowlist_while_repo_sets_a_nonempty_list_is_a_hard_error() {
        let repo = table(
            "[policy]\nnetwork_allowlist = [{ scheme = \"https\", host = \"api.example.com\" }]\n",
        )
        .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[]);
        let err = resolve(None, repo, &|k| vars.get(k).cloned())
            .expect_err("a repo may not name any host when the operator granted none");
        assert!(
            err.to_string().contains("api.example.com"),
            "the error must name the offending host: {err}"
        );
    }

    /// The acceptance criterion in its most literal form: with no
    /// `network_allowlist` configured anywhere (today's only shipped state),
    /// both the report claude's `policy_support` path produces AND the real
    /// argv `policy_args` emits are byte-identical to before this field
    /// existed -- an empty allowlist changes nothing.
    #[test]
    fn an_empty_network_allowlist_leaves_claudes_report_and_argv_unaffected() {
        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        assert!(policy.network_allowlist.is_empty());
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let network_outcome = report
            .outcomes
            .iter()
            .find(|outcome| outcome.capability == Capability::Network)
            .expect("network row present when network is Some");
        assert_eq!(network_outcome.support, Support::OperatorControlled);
        assert_eq!(
            claude.policy_args(&policy, adapters::LaunchMode::Interactive),
            Vec::<String>::new(),
            "an empty allowlist must not change policy_args' argv at all"
        );
    }

    /// Claude's honest answer once an operator actually configures a
    /// non-empty allowlist: the mechanism must name its own actual permission
    /// rule syntax and the gap it leaves (Bash is unscoped); it must never
    /// claim `Enforced`. Round 2 (issue #727): the mechanism string is now a
    /// generic, static description of the mechanism -- the actual configured
    /// host is no longer duplicated into it (round 1's `Box::leak`'d,
    /// per-call string); the real per-host rules live in the launch argv
    /// `ClaudeAdapter::default_sandbox_args` builds, exercised by
    /// `default_sandbox_args_replaces_the_wholesale_webfetch_websearch_
    /// allow_with_per_host_rules` in `adapters/claude.rs`.
    #[test]
    fn claude_reports_a_configured_allowlist_as_degraded_and_names_the_bash_gap() {
        let claude = ClaudeAdapter::new(None);
        let allowlist = vec![NetworkTarget {
            scheme: "https".to_string(),
            host: "api.example.com".to_string(),
            port: None,
        }];
        let descriptor = claude.network_allowlist_support(
            &allowlist,
            Stance::Allow,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert_ne!(descriptor.support, Support::Enforced);
        assert!(
            descriptor.mechanism.contains("WebFetch(domain:<host>)"),
            "must name claude's own permission-rule syntax: {}",
            descriptor.mechanism
        );
        assert!(
            descriptor.mechanism.contains("Bash"),
            "must name the Bash-scoping gap: {}",
            descriptor.mechanism
        );
    }

    /// The same answer, reached through `evaluate` end to end (not by calling
    /// the adapter method directly): a configured allowlist with `network`
    /// not denied renders as `Degraded` in the actual report, never
    /// `Enforced` -- `zirv ctx status`/`policy` must never claim more than
    /// this mechanism delivers.
    #[test]
    fn evaluate_renders_a_configured_allowlist_as_degraded_never_enforced() {
        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            network_allowlist: vec![NetworkTarget {
                scheme: "https".to_string(),
                host: "api.example.com".to_string(),
                port: None,
            }],
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let network_outcome = report
            .outcomes
            .iter()
            .find(|outcome| outcome.capability == Capability::Network)
            .expect("network row present");
        assert_eq!(network_outcome.support, Support::Degraded);
        assert_ne!(network_outcome.support, Support::Enforced);
        assert!(
            network_outcome
                .mechanism
                .contains("WebFetch(domain:<host>)")
        );
    }

    /// A `Deny` network stance leaves nothing for a host allowlist to scope:
    /// `evaluate` must not reach `network_allowlist_support` at all here, so
    /// the report is unchanged from before this field existed (claude's
    /// plain `Unsupported` for a denied network).
    #[test]
    fn a_denied_network_stance_ignores_the_allowlist_entirely() {
        let policy = EffectivePolicy {
            network: Some(Stance::Deny),
            network_allowlist: vec![NetworkTarget {
                scheme: "https".to_string(),
                host: "api.example.com".to_string(),
                port: None,
            }],
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let network_outcome = report
            .outcomes
            .iter()
            .find(|outcome| outcome.capability == Capability::Network)
            .expect("network row present");
        assert_eq!(network_outcome.support, Support::Unsupported);
    }

    /// The operator's escape hatch above the fold, mirroring
    /// `ZIRV_AGENT_<NAME>_ENABLED`: the environment wins outright, including
    /// in the loosening direction a repo file can never take.
    #[test]
    fn the_environment_sits_above_the_fold_in_both_directions() {
        let repo = table("[policy]\nshell_exec = \"deny\"\nnetwork = \"deny\"\n")
            .and_then(|v| v.get("policy").cloned());
        let vars = env_from(&[("ZIRV_CTX_POLICY_SHELL_EXEC", "allow")]);
        let resolved = resolve(None, repo, &|k| vars.get(k).cloned()).expect("resolves");
        assert_eq!(resolved.shell_exec, Stance::Allow);
        assert_eq!(resolved.network, Some(Stance::Deny));
    }

    #[test]
    fn an_unparseable_stance_is_an_error_not_a_silent_allow() {
        let vars = env_from(&[("ZIRV_CTX_POLICY_NETWORK", "maybe")]);
        let err = resolve(None, None, &|k| vars.get(k).cloned()).expect_err("must reject");
        assert!(
            err.to_string().contains("ZIRV_CTX_POLICY_NETWORK"),
            "error should name the variable: {err}"
        );

        let bad_file =
            table("[policy]\nnetwork = \"sometimes\"\n").and_then(|v| v.get("policy").cloned());
        let empty = env_from(&[]);
        let err = resolve(bad_file, None, &|k| empty.get(k).cloned()).expect_err("must reject");
        assert!(
            err.to_string().contains("[policy]"),
            "error should name the section: {err}"
        );
    }

    #[test]
    fn a_typoed_capability_name_is_rejected_rather_than_ignored() {
        let bad = table("[policy]\nshel_exec = \"deny\"\n").and_then(|v| v.get("policy").cloned());
        let empty = env_from(&[]);
        assert!(resolve(bad, None, &|k| empty.get(k).cloned()).is_err());
    }

    /// The honesty rule at its most load-bearing: an `Allow` capability is
    /// never reported as something zirv enforces, because zirv is not
    /// restricting anything. `network` is pinned explicitly to `Allow` here
    /// rather than left at `EffectivePolicy::default()`'s own `Deny`: this
    /// test is about the `Allow` stance specifically, on every capability,
    /// not about the default policy.
    #[test]
    fn an_allow_stance_reports_as_operator_controlled_on_every_adapter() {
        let policy = EffectivePolicy {
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        for adapter in adapters::all(None) {
            let report = evaluate(&policy, adapter.as_ref(), adapters::LaunchMode::Headless);
            for outcome in &report.outcomes {
                assert_eq!(
                    outcome.support,
                    Support::OperatorControlled,
                    "{} / {} should be operator-controlled at allow",
                    report.adapter,
                    outcome.capability.key()
                );
            }
            assert!(report.unenforced().is_empty());
        }
    }

    /// Every registered adapter answers every capability at every restrictive
    /// stance, and a report always covers the whole policy -- no capability
    /// silently omitted because an adapter had nothing to say about it.
    #[test]
    fn every_adapter_answers_every_capability_at_every_restrictive_stance() {
        for stance in [Stance::Ask, Stance::Deny] {
            let mut policy = EffectivePolicy::default();
            for capability in Capability::ALL {
                if capability == Capability::Network {
                    policy.network = Some(stance);
                    continue;
                }
                *policy.stance_mut(capability) = stance;
            }
            for adapter in adapters::all(None) {
                let report = evaluate(&policy, adapter.as_ref(), adapters::LaunchMode::Headless);
                assert_eq!(report.outcomes.len(), Capability::ALL.len());
                for outcome in &report.outcomes {
                    assert_eq!(outcome.stance, stance);
                    assert!(
                        !outcome.mechanism.is_empty(),
                        "{} / {} must name a mechanism or say why there is none",
                        report.adapter,
                        outcome.capability.key()
                    );
                }
            }
        }
    }

    /// The honesty rule as a type-level property: nothing that is only
    /// advisory may render as enforced, and `Unsupported`'s own wording has to
    /// say so out loud.
    #[test]
    fn an_unsupported_capability_never_renders_as_enforcement() {
        assert!(Support::Unsupported.label().contains("not enforced"));
        assert!(Support::Unsupported.label().contains("advisory"));
        assert!(
            CapabilityDescriptor::advisory_only()
                .mechanism
                .contains("not enforcement")
        );
    }

    /// Only `Enforced` means the requested stance itself is fully met, with
    /// nothing left for prompt text or the operator's own settings to carry
    /// -- see `is_fully_enforced`'s own doc for why the looser
    /// `Enforced`-or-`Degraded` question this replaced is gone rather than
    /// kept alongside it.
    #[test]
    fn only_enforced_is_fully_enforced() {
        assert!(Support::Enforced.is_fully_enforced());
        assert!(!Support::Degraded.is_fully_enforced());
        assert!(!Support::Unsupported.is_fully_enforced());
        assert!(!Support::OperatorControlled.is_fully_enforced());
    }

    /// A rendered report names the stance, the honest state and the mechanism
    /// on every line -- the three facts an operator needs to tell a real
    /// guarantee from an instruction.
    #[test]
    fn a_rendered_report_names_stance_state_and_mechanism_per_line() {
        let policy = EffectivePolicy {
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let rendered = evaluate(&policy, &claude, adapters::LaunchMode::Headless).render();
        assert!(rendered.contains("shell execution: deny -- enforced"));
        assert!(rendered.contains("--disallowedTools=Write,Edit,Bash,NotebookEdit"));
    }

    /// Claude's verified mechanisms, from
    /// docs/superpowers/notes/2026-08-01-system-prompt-injection-facts.md: the
    /// `--disallowedTools` pin is the one thing zirv can hold a claude launch
    /// to, and it only fully covers repo writes and shell execution -- the
    /// two capabilities the four denied tools (`Write`/`Edit`/`Bash`/
    /// `NotebookEdit`) actually deny outright. Tool access is only
    /// `Degraded` (the pin denies four tools, not every tool -- `Read`,
    /// `Grep`, `WebFetch`, `WebSearch`, `Task` and MCP tools remain), and
    /// approval is `Unsupported` (the pin does not address approvals at
    /// all): claiming either as `Enforced` over-claims what a verified
    /// four-tool pin can back.
    #[test]
    fn claude_enforces_what_its_verified_tool_pin_covers_and_nothing_else() {
        let claude = ClaudeAdapter::new(None);
        let enforced = |capability| {
            claude
                .policy_support(capability, Stance::Deny, adapters::LaunchMode::Headless)
                .support
        };
        assert_eq!(enforced(Capability::RepoFsWrite), Support::Enforced);
        assert_eq!(enforced(Capability::ShellExec), Support::Enforced);
        assert_eq!(enforced(Capability::ToolAccess), Support::Degraded);
        assert_eq!(enforced(Capability::Approval), Support::Unsupported);
        assert_eq!(enforced(Capability::Network), Support::Unsupported);
        assert_eq!(
            enforced(Capability::GitPushDestructive),
            Support::Unsupported
        );
        assert_eq!(
            enforced(Capability::OutsideRepoFsWrite),
            Support::Unsupported
        );
    }

    /// The tool-access `Degraded` mechanism must actually name what remains
    /// available, not just what is denied -- otherwise an operator reads
    /// "degraded" without learning that Read/Grep/WebFetch/WebSearch/Task and
    /// every MCP server's tools are still reachable.
    #[test]
    fn claude_tool_access_degraded_mechanism_names_what_still_runs() {
        let claude = ClaudeAdapter::new(None);
        let descriptor = claude.policy_support(
            Capability::ToolAccess,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert!(descriptor.mechanism.contains("Write"));
        assert!(descriptor.mechanism.contains("MCP"));
    }

    /// Approval has no verified per-run mechanism on claude at all -- the
    /// four-tool pin never addresses approvals, so it must not be reported
    /// as even `Degraded`.
    #[test]
    fn claude_approval_at_deny_is_unsupported_not_enforced() {
        let claude = ClaudeAdapter::new(None);
        let descriptor = claude.policy_support(
            Capability::Approval,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Unsupported);
        assert!(descriptor.mechanism.contains("approval"));
    }

    /// An `Ask` stance is a different question from a `Deny` one: claude's
    /// verified per-run pin can only deny outright, so asking for "ask"
    /// lands on the operator's own settings rather than on a zirv guarantee.
    /// `--permission-mode plan` was probed and does not resolve in headless
    /// `-p` mode, so it is not claimed here.
    #[test]
    fn claude_does_not_claim_to_pin_an_ask_stance() {
        let claude = ClaudeAdapter::new(None);
        for capability in Capability::ALL {
            let descriptor =
                claude.policy_support(capability, Stance::Ask, adapters::LaunchMode::Headless);
            assert!(
                !matches!(descriptor.support, Support::Enforced | Support::Degraded),
                "{} must not claim a per-run ask mechanism, not even a degraded one",
                capability.key()
            );
        }
    }

    /// Codex's descriptors come from the repo's recorded facts
    /// (docs/superpowers/notes/2026-07-31-codex-cli-facts.md), not from a live
    /// CLI -- codex is not runnable on this machine. The read-only sandbox
    /// and state-dir write profile scope what
    /// an executed shell command may touch rather than which of codex's tools
    /// may run, so every stance it carries is `Degraded`, never `Enforced`.
    #[test]
    fn codex_never_claims_full_enforcement_from_its_sandbox_pin() {
        let codex = CodexAdapter::new(None);
        for capability in Capability::ALL {
            for stance in [Stance::Ask, Stance::Deny] {
                let descriptor =
                    codex.policy_support(capability, stance, adapters::LaunchMode::Headless);
                assert_ne!(
                    descriptor.support,
                    Support::Enforced,
                    "codex must not claim full enforcement for {} at {}",
                    capability.key(),
                    stance.label()
                );
            }
        }
        assert_eq!(
            codex
                .policy_support(
                    Capability::RepoFsWrite,
                    Stance::Deny,
                    adapters::LaunchMode::Headless,
                )
                .support,
            Support::Degraded
        );
        assert!(
            codex
                .policy_support(
                    Capability::RepoFsWrite,
                    Stance::Deny,
                    adapters::LaunchMode::Headless,
                )
                .mechanism
                .contains("-c sandbox_mode=\"read-only\" plus the zirv-read-only permissions profile (writes allowed only to the zirv state directory)")
        );
    }

    /// `--sandbox read-only` scopes writes, not execution -- a command still
    /// runs under it and can read anything the process can reach. Shell
    /// execution at `Deny` must therefore be `Unsupported`, not `Degraded`:
    /// reporting `Degraded` would claim the sandbox restricts *something*
    /// about whether commands run, which it does not.
    #[test]
    fn codex_shell_exec_at_deny_is_unsupported_not_degraded() {
        let codex = CodexAdapter::new(None);
        let descriptor = codex.policy_support(
            Capability::ShellExec,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Unsupported);
        assert!(descriptor.mechanism.contains("write"));
    }

    /// Revised 2026-08-22: `-a, --ask-for-approval never` is real and
    /// verified against the installed `codex-cli 0.147.0` (the original
    /// `Unsupported` verdict here predates that finding -- see the
    /// 2026-08-22 addendum to `docs/superpowers/notes/2026-07-31-codex-cli-
    /// facts.md`). Not `Enforced`: in isolation it only suppresses the
    /// escalation prompt, it does not by itself decide what the sandbox
    /// blocks -- see `CodexAdapter::policy_support`'s own doc comment for
    /// why the pairing with `--sandbox read-only` is what actually closes
    /// the loop. `Approval` at `Deny` is therefore `Degraded`, not
    /// `Unsupported` or `Enforced`.
    #[test]
    fn codex_approval_at_deny_is_degraded_not_unsupported_or_enforced() {
        let codex = CodexAdapter::new(None);
        let descriptor = codex.policy_support(
            Capability::Approval,
            Stance::Deny,
            adapters::LaunchMode::Headless,
        );
        assert_eq!(descriptor.support, Support::Degraded);
        assert!(descriptor.mechanism.contains("ask-for-approval"));
    }

    /// Codex has no verified per-tool deny and no verified network control, so
    /// those stay advisory -- the asymmetry with claude is reported, not
    /// smoothed over.
    #[test]
    fn codex_reports_its_unverified_capabilities_as_advisory_only() {
        let codex = CodexAdapter::new(None);
        for capability in [
            Capability::Network,
            Capability::ToolAccess,
            Capability::GitPushDestructive,
        ] {
            assert_eq!(
                codex
                    .policy_support(capability, Stance::Deny, adapters::LaunchMode::Headless)
                    .support,
                Support::Unsupported,
                "{} should be advisory-only on codex",
                capability.key()
            );
        }
    }

    /// One canonical policy, evaluated against both harnesses, produces two
    /// different honest answers -- issue #43's acceptance criterion, and the
    /// reason the policy is not written per harness in the first place.
    /// Claude fully enforces `repo_fs_write` at `Deny` (its four-tool pin
    /// denies `Write`/`Edit` outright); codex only degrades it (its sandbox
    /// flag scopes writes, it does not deny a tool), so codex's report must
    /// still surface that capability as unenforced even though a real,
    /// verified mechanism is doing something.
    #[test]
    fn one_policy_evaluates_differently_against_claude_and_codex() {
        let policy = EffectivePolicy {
            repo_fs_write: Stance::Deny,
            // Pinned explicitly to `Allow` (not left at `default()`'s own
            // `Deny`): this test is about `repo_fs_write` alone, and network
            // at its own default would also show up as claude-unenforced
            // (advisory-only), which is not what this test pins.
            network: Some(Stance::Allow),
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let codex = CodexAdapter::new(None);
        let claude_report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let codex_report = evaluate(&policy, &codex, adapters::LaunchMode::Headless);
        assert_ne!(claude_report.outcomes, codex_report.outcomes);
        assert!(claude_report.unenforced().is_empty());
        let codex_unenforced: Vec<_> = codex_report
            .unenforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(codex_unenforced, vec![Capability::RepoFsWrite]);
        let codex_partial: Vec<_> = codex_report
            .partially_enforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(codex_partial, vec![Capability::RepoFsWrite]);
    }

    /// The lines an operator has to read: a stance zirv asked for that only
    /// prompt text or the operator's own harness settings are carrying.
    #[test]
    fn unenforced_lists_exactly_the_stances_zirv_cannot_hold_the_harness_to() {
        let policy = EffectivePolicy {
            network: Some(Stance::Deny),
            shell_exec: Stance::Deny,
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);
        let unenforced: Vec<_> = report
            .unenforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(unenforced, vec![Capability::Network]);
    }

    /// `partially_enforced` must isolate exactly the `Degraded` cells, not
    /// every unenforced one: claude's tool-access pin is `Degraded` at
    /// `Deny`, but its approval and network answers are `Unsupported`, which
    /// must not show up here even though both also appear in `unenforced`.
    #[test]
    fn partially_enforced_lists_only_the_degraded_cells() {
        let policy = EffectivePolicy {
            tool_access: Stance::Deny,
            approval: Stance::Deny,
            network: Some(Stance::Deny),
            ..EffectivePolicy::default()
        };
        let claude = ClaudeAdapter::new(None);
        let report = evaluate(&policy, &claude, adapters::LaunchMode::Headless);

        let partial: Vec<_> = report
            .partially_enforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(partial, vec![Capability::ToolAccess]);

        let unenforced: Vec<_> = report
            .unenforced()
            .iter()
            .map(|outcome| outcome.capability)
            .collect();
        assert_eq!(
            unenforced,
            vec![
                Capability::Network,
                Capability::Approval,
                Capability::ToolAccess
            ],
            "unenforced must still include the Degraded cell, not just the Unsupported ones"
        );
    }

    /// `Capability::ALL` is hand-maintained, and so is the match below -- so
    /// what does this actually catch?
    ///
    /// The match is exhaustive (no wildcard arm): the moment `Capability`
    /// gains a new variant, this file fails to compile until an arm exists
    /// for it here, one line saying which position that variant belongs at
    /// in `Capability::ALL`. The test then confirms each entry *already in*
    /// `Capability::ALL` sits at the exact position its own arm claims, and
    /// that no two entries claim the same position -- so a variant that got
    /// **duplicated or reordered** relative to `Capability::ALL` (the
    /// realistic way this list actually drifts: copy-pasting an existing arm
    /// instead of adding a fresh one, or an entry moved without updating its
    /// neighbours) is caught.
    ///
    /// What it provably does **not** catch: a variant added to the enum,
    /// given its own honest arm here, but never appended to `Capability::ALL`
    /// at all. This function is only ever called with values already drawn
    /// from `Capability::ALL`'s own contents, so an arm for a variant absent
    /// from that array is simply never exercised -- no test in this file can
    /// call `capability_all_index` with a variant it has no way to name
    /// without already knowing about the very omission it would need to
    /// detect. Closing that gap for real needs either a derive macro (e.g.
    /// `strum::EnumIter`) or nightly's unstable `variant_count`, neither of
    /// which this fix pulls in -- so a brand new variant appended to
    /// `Capability` and never added to `Capability::ALL` still relies on
    /// code review, not this test, to be caught.
    #[test]
    fn capability_all_entries_are_at_their_declared_position_with_no_duplicates() {
        fn capability_all_index(capability: Capability) -> usize {
            match capability {
                Capability::RepoFsWrite => 0,
                Capability::OutsideRepoFsWrite => 1,
                Capability::ShellExec => 2,
                Capability::Network => 3,
                Capability::Approval => 4,
                Capability::GitPushDestructive => 5,
                Capability::ToolAccess => 6,
            }
        }

        let mut claimed_positions = std::collections::HashSet::new();
        for (position, &capability) in Capability::ALL.iter().enumerate() {
            let claimed = capability_all_index(capability);
            assert_eq!(
                claimed, position,
                "{capability:?} is at Capability::ALL[{position}] but claims position {claimed} \
                 -- reordered, or a stale/duplicated entry"
            );
            assert!(
                claimed_positions.insert(claimed),
                "{capability:?}'s position {claimed} is claimed by more than one Capability::ALL \
                 entry"
            );
        }
    }
}
