use super::*;

/// Operator-only worker model overrides; unset values use adapter defaults and every model passes the argv guard.
/// Repos cannot choose the vendor account a worker spends; envelope defaults share this table (#262).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
    /// Codex worker reasoning effort, passed as `-c model_reasoning_effort=`; unset leaves Codex's own default.
    pub codex_effort: Option<String>,
    /// Operator-only root delegation depth; repos cannot grant themselves more reach (#262).
    pub default_depth: u8,
    /// Operator-only root read-only default: no destructive access or write roots (#262).
    pub default_read_only: bool,
    /// Envelope depth ceiling; repos may only lower it, regardless of defaults or requested depth (#262).
    pub max_depth: u8,
    /// Deny network tools in every envelope; repos may enable denial but never remove it (#262).
    pub deny_network: bool,
    /// Operator-only bootstrap deadline; repos cannot lengthen it to spend more of the operator account.
    pub bootstrap_timeout_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            claude: None,
            codex: None,
            codex_effort: None,
            default_depth: 1,
            default_read_only: false,
            max_depth: u8::MAX,
            deny_network: false,
            bootstrap_timeout_secs: 600,
        }
    }
}

/// Idle worktree count and lifetime; repos may only shrink either bound (#718).
/// GC and reconciliation still require removal proof through `prune_one`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    pub idle_pool_max: u32,
    pub idle_ttl_secs: u64,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            idle_pool_max: 4,
            idle_ttl_secs: 3600,
        }
    }
}

/// Objective completion controls: repos may only drop gates without reordering, lower cycles, or disable the judge (#314).
/// Enabling a model judge over untrusted transcripts is a widening reserved to the operator.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObjectiveConfig {
    pub gates: Vec<String>,
    pub max_cycles_without_progress: u32,
    pub judge: bool,
}

impl Default for ObjectiveConfig {
    fn default() -> Self {
        Self {
            gates: vec!["zirv test changed".to_string(), "zirv verify".to_string()],
            max_cycles_without_progress: 5,
            judge: true,
        }
    }
}

/// Repos may only lower repetition thresholds to tighten detection (#272, #322).
/// Pass resolved thresholds into `screen.rs` so it remains pure and never reads config.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenConfig {
    pub repetition_min_fragment: u32,
    pub repetition_window: u32,
    pub repetition_min_repeats: u32,
    pub repetition_dominance_pct: f64,
}

/// Operator-only opt-in treatment of sensitive text before model/network boundaries; off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscateMode {
    #[default]
    Off,
    Flag,
    Obfuscate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscateEntropy {
    #[default]
    Flag,
    Obfuscate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscatePrompt {
    #[default]
    Flag,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscateEmailDomain {
    #[default]
    Keep,
    Mask,
}

/// Reduced motion preserves state changes and supervision while removing animation; presentation is repo-settable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DashMotion {
    #[default]
    Full,
    Reduced,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObfuscatePatternConfig {
    pub kind: String,
    pub regex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObfuscateConfig {
    pub mode: ObfuscateMode,
    pub entropy: ObfuscateEntropy,
    pub prompt: ObfuscatePrompt,
    pub email_domain: ObfuscateEmailDomain,
    pub patterns: Vec<ObfuscatePatternConfig>,
    pub literals_file: Option<String>,
    pub allow: Vec<String>,
    #[serde(skip)]
    pub(crate) operator_load_failed: bool,
}

impl ObfuscateConfig {
    pub(super) fn load_operator_only(env: EnvLookup<'_>) -> CtxResult<Self> {
        load_operator_section(env, "obfuscate")
    }

    pub(super) fn fail_closed() -> Self {
        // Unreadable operator policy must fail option loading (#466).
        Self {
            mode: ObfuscateMode::Obfuscate,
            prompt: ObfuscatePrompt::Block,
            operator_load_failed: true,
            ..Self::default()
        }
    }

    pub fn options(&self, literals: Vec<String>) -> super::super::obfuscate::Options {
        super::super::obfuscate::Options {
            mode: match self.mode {
                ObfuscateMode::Off => super::super::obfuscate::Mode::Off,
                ObfuscateMode::Flag => super::super::obfuscate::Mode::Flag,
                ObfuscateMode::Obfuscate => super::super::obfuscate::Mode::Obfuscate,
            },
            entropy: match self.entropy {
                ObfuscateEntropy::Flag => super::super::obfuscate::EntropyMode::Flag,
                ObfuscateEntropy::Obfuscate => super::super::obfuscate::EntropyMode::Obfuscate,
            },
            email_domain: match self.email_domain {
                ObfuscateEmailDomain::Keep => super::super::obfuscate::EmailDomain::Keep,
                ObfuscateEmailDomain::Mask => super::super::obfuscate::EmailDomain::Mask,
            },
            patterns: self
                .patterns
                .iter()
                .map(|pattern| super::super::obfuscate::OperatorPattern {
                    kind: pattern.kind.clone(),
                    regex: pattern.regex.clone(),
                })
                .collect(),
            literals,
            allow: self.allow.clone(),
        }
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        let defaults = super::super::screen::Thresholds::default();
        Self {
            repetition_min_fragment: defaults.repetition_min_fragment as u32,
            repetition_window: defaults.repetition_window as u32,
            repetition_min_repeats: defaults.repetition_min_repeats as u32,
            repetition_dominance_pct: defaults.repetition_dominance_pct,
        }
    }
}

impl ScreenConfig {
    /// Resolve config into explicit inputs so `screen.rs` remains pure.
    pub fn thresholds(&self) -> super::super::screen::Thresholds {
        super::super::screen::Thresholds {
            repetition_min_fragment: self.repetition_min_fragment as usize,
            repetition_window: self.repetition_window as usize,
            repetition_min_repeats: self.repetition_min_repeats as usize,
            repetition_dominance_pct: self.repetition_dominance_pct,
        }
    }
}

/// Optional literal model ids for the three handover tiers; unset values use the built-in ladder.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoverTierConfig {
    pub cheap: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
}

/// Operator-only handover model tiers; environment overrides win (#84).
/// Repos cannot choose which vendor account the orchestrator spends.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoverConfig {
    pub claude: HandoverTierConfig,
    pub codex: HandoverTierConfig,
}

/// Explicit seat-tier mappings; unset means pass no model, never substitute a cheaper tier (#699).
/// Only the manifest's exact tier may be selected; there is no built-in fallback ladder.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTierConfig {
    pub fast: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
}

/// Operator-only `[model_tiers.<adapter>]` fast/standard/deep mappings; unset values choose no model (#699).
/// A repo-selected provider/model is a widening, so the entire table is forbidden to repos.
/// Older versions reject persisted unknown keys through `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTiersConfig {
    pub claude: ModelTierConfig,
    pub codex: ModelTierConfig,
}

/// Operator-only compatible endpoint; unknown catalogue lookups fall back to the native provider without panicking (#395).
/// `credential_env` names an API-key variable read at launch; its value is never logged, printed or persisted.
/// `model` defaults to the strongest vendor rung and is mandatory for rungless vendors; Codex wire API is chat or responses.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointTarget {
    pub vendor: String,
    pub base_url: String,
    pub credential_env: String,
    pub model: Option<String>,
    pub wire_api: Option<String>,
}

impl EndpointTarget {
    /// Honor requested models only on this vendor's ladder; foreign aliases fall back to the endpoint default.
    pub fn pin_model(&self, requested: Option<&str>) -> String {
        if let Some(model) = requested
            && let Some(vendor) = super::super::catalogue::vendor(&self.vendor)
            && super::super::catalogue::rung_of_known(vendor, model).is_some()
        {
            return model.to_string();
        }
        self.default_model()
    }

    /// Loaded configs always supply a default via an explicit model or the vendor's strongest rung.
    fn default_model(&self) -> String {
        if let Some(model) = self.model.as_deref() {
            return model.to_string();
        }
        super::super::catalogue::vendor(&self.vendor)
            .and_then(|v| v.rungs.first())
            .map(|r| r.id.to_string())
            .unwrap_or_default()
    }
}

/// Operator-only endpoint overrides: repos cannot select the vendor account a harness spends (#395).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointConfig {
    pub claude: Option<EndpointTarget>,
    pub codex: Option<EndpointTarget>,
}

/// Default launch posture is sandboxed with no prompts; out-of-workspace commands fail (#83).
/// The baseline is independent of `[policy]`; repos cannot disable sandboxing or widen `extra_allow`.
/// `extra_deny` unions home and repo entries so repo arrays cannot erase operator denials; env overrides win outright.
/// Extra rules use Claude permission syntax; Codex has no per-command equivalent, and deny always beats allow.
/// Subprocess env scrubbing is operator-only and off by default: it strips tool/auth env and forces default permission mode (#329).
/// Sandbox filesystem and Read denials still block sensitive files when scrubbing is off.
/// `scrub_worker_secrets` (on by default, operator-only) strips secret-shaped env from delegated workers, keeping the harness's own credentials.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub extra_allow: Vec<String>,
    pub extra_deny: Vec<String>,
    pub scrub_subprocess_env: bool,
    pub scrub_worker_secrets: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_allow: Vec::new(),
            extra_deny: Vec::new(),
            scrub_subprocess_env: false,
            scrub_worker_secrets: true,
        }
    }
}

/// Repos may only reduce fallback eagerness: disable, drop candidates, demand headroom or lower capacity/task bounds (#186).
/// Environment variables remain the operator's final word.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackConfig {
    /// Enable automatic routing and blocked-session continuation.
    pub enabled: bool,
    /// Tie-break preference after known or assumed headroom.
    pub order: Vec<String>,
    /// Reroute new work when the requested harness falls below this headroom percentage.
    pub predictive_headroom_pct: f64,
    /// A fallback candidate must have at least this much percentage headroom.
    pub min_candidate_headroom_pct: f64,
    /// Conservative headroom when usage is absent or stale; zero excludes those harnesses.
    pub unknown_headroom_pct: f64,
    /// Small-capacity harnesses require an explicit budget at or below this limit or the tool-call limit.
    pub small_task_max_tokens: u64,
    /// Alternative small-task bound; at least one explicit bounded dimension is required.
    pub small_task_max_tool_calls: u32,
    /// Adaptive background routing as headroom changes; repos may disable but never enable it (#358).
    pub adaptive_delegation: bool,
    /// Unset enables orchestrator rollover only when multiple fallback harnesses are enabled (#358).
    /// Explicit operator values win; repos may disable rollover but cannot enable it, even when home is unset.
    pub auto_orchestrator_rollover: Option<bool>,
    /// Operator-only rollover threshold, defaulting to predictive headroom; repos cannot tune seat swaps (#358).
    pub orchestrator_rollover_headroom_pct: Option<f64>,
    /// Operator-only minimum rollover gap prevents seat thrashing near the threshold (#358).
    pub rollover_cooldown_secs: u64,
    /// Maximum idle wait after a confirmed block; only the operator may authorize a forced seat swap.
    pub reactive_force_after_secs: u64,
    /// Per-harness limits: repos may only lower max-active or raise reserved headroom (#358).
    pub harness: std::collections::BTreeMap<String, HarnessLimits>,
    /// Route health catches connection failures that usage cannot express (#455).
    /// Repos may disable the breaker, but only the operator may tune timings that govern spend routing.
    pub health: super::super::health::HealthPolicy,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            order: vec!["claude".to_string(), "codex".to_string()],
            predictive_headroom_pct: 20.0,
            min_candidate_headroom_pct: 10.0,
            unknown_headroom_pct: 25.0,
            small_task_max_tokens: 40_000,
            small_task_max_tool_calls: 24,
            adaptive_delegation: true,
            auto_orchestrator_rollover: None,
            orchestrator_rollover_headroom_pct: None,
            rollover_cooldown_secs: 600,
            reactive_force_after_secs: 120,
            harness: std::collections::BTreeMap::new(),
            health: super::super::health::HealthPolicy::default(),
        }
    }
}

impl FallbackConfig {
    /// Use the explicit rollover threshold or inherit predictive headroom (#358).
    pub fn rollover_headroom_pct(&self) -> f64 {
        self.orchestrator_rollover_headroom_pct
            .unwrap_or(self.predictive_headroom_pct)
    }

    /// Match harness names case-insensitively; missing entries inherit global limits (#358).
    pub fn harness_limits(&self, name: &str) -> HarnessLimits {
        self.harness
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, limits)| limits.clone())
            .unwrap_or_default()
    }

    /// Use the harness reserve override or the global candidate floor (#358).
    pub fn reserve_headroom_pct(&self, name: &str) -> f64 {
        self.harness_limits(name)
            .reserve_headroom_pct
            .unwrap_or(self.min_candidate_headroom_pct)
    }

    /// Disable the breaker with fallback: denying work is unsafe when no reroute is available (#455).
    pub fn effective_health(&self) -> super::super::health::HealthPolicy {
        super::super::health::HealthPolicy {
            enabled: self.enabled && self.health.enabled,
            ..self.health.clone()
        }
    }
}

/// Unset harness limits inherit global settings; repos may only lower max-active or raise headroom (#358).
/// Repo-only max-active narrows unlimited capacity; repo-only headroom must still meet the global floor.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessLimits {
    /// Optional concurrent-delegation ceiling; unset adds no per-harness cap.
    pub max_active: Option<u32>,
    /// Optional reserve-headroom override; unset uses the global candidate floor.
    pub reserve_headroom_pct: Option<f64>,
}

/// Operator-only runtime defaults; absent or unrecognized values resolve to harness (#491).
/// Neither backend switch is repo narrowing: both can spend the operator's account.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Native or harness; keep unknown values as strings so older builds fall back with a diagnosis instead of aborting.
    pub default: Option<String>,
    /// Overrides keyed by native role names, taking precedence over `default`.
    pub roles: std::collections::BTreeMap<String, String>,
}

/// Operator-only native integrations: repos cannot add executable commands, network endpoints or auth references (#483).
/// Off by default; unconfigured capabilities report unavailable with a diagnosis, never empty success.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitiesConfig {
    /// When disabled, never contact MCP servers, make web calls or launch browsers.
    pub enabled: bool,
    pub web: WebCapabilityConfig,
    pub browser: BrowserCapabilityConfig,
    /// Configured MCP servers, in declaration order.
    pub mcp: Vec<McpServerConfig>,
    /// Inline tools only below this count; larger catalogues use on-demand schemas to bound model requests.
    pub max_inline_mcp_tools: usize,
}

impl CapabilitiesConfig {
    pub const DEFAULT_MAX_INLINE_MCP_TOOLS: usize = 24;

    pub fn max_inline_mcp_tools_or_default(&self) -> usize {
        if self.max_inline_mcp_tools == 0 {
            Self::DEFAULT_MAX_INLINE_MCP_TOOLS
        } else {
            self.max_inline_mcp_tools
        }
    }

    /// Disabled servers must remain out of every session.
    pub fn active_servers(&self) -> impl Iterator<Item = &McpServerConfig> {
        let enabled = self.enabled;
        self.mcp
            .iter()
            .filter(move |server| enabled && server.enabled && !server.name.trim().is_empty())
    }
}

/// Web search and fetch require explicit configuration; a raw model provides neither.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebCapabilityConfig {
    /// JSON search endpoint with a `{query}` URL template; unset means unavailable, never empty success.
    pub search_endpoint: Option<String>,
    /// `env:NAME`, `store:<item>` or `file:<path>` auth reference; never logged.
    pub search_credential: Option<String>,
    pub fetch_enabled: bool,
    /// Allowed hosts; an empty list denies all access.
    pub allow_hosts: Vec<String>,
    /// Bound each fetched body before storing evidence.
    pub max_fetch_bytes: usize,
    pub timeout_ms: u64,
}

impl WebCapabilityConfig {
    pub const DEFAULT_MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
    pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;

    pub fn max_fetch_bytes_or_default(&self) -> usize {
        if self.max_fetch_bytes == 0 {
            Self::DEFAULT_MAX_FETCH_BYTES
        } else {
            self.max_fetch_bytes
        }
    }

    pub fn timeout_ms_or_default(&self) -> u64 {
        if self.timeout_ms == 0 {
            Self::DEFAULT_TIMEOUT_MS
        } else {
            self.timeout_ms
        }
    }
}

/// Use the frontend renderer's Chromium backend so the same installation supports native browsing.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserCapabilityConfig {
    pub enabled: bool,
    /// Explicit browser binary or PATH discovery; no binary means unavailable.
    pub binary: Option<String>,
    pub timeout_ms: u64,
}

impl BrowserCapabilityConfig {
    pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

    pub fn timeout_ms_or_default(&self) -> u64 {
        if self.timeout_ms == 0 {
            Self::DEFAULT_TIMEOUT_MS
        } else {
            self.timeout_ms
        }
    }
}

/// MCP effects are trusted operator declarations used by the broker; server descriptions never grant authority.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransportConfig,
    pub effects: CapabilityEffectsConfig,
    pub request_timeout_ms: u64,
}

impl McpServerConfig {
    pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

    pub fn request_timeout_ms_or_default(&self) -> u64 {
        if self.request_timeout_ms == 0 {
            Self::DEFAULT_REQUEST_TIMEOUT_MS
        } else {
            self.request_timeout_ms
        }
    }
}

/// Local or remote mode must be explicit; a parsable URL never selects the transport.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransportConfig {
    /// A child process speaking newline-delimited JSON-RPC on stdin/stdout.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
    /// Streamable HTTP with optional bearer authentication from the shared store.
    Http {
        url: String,
        #[serde(default)]
        credential: Option<String>,
    },
}

impl Default for McpTransportConfig {
    fn default() -> Self {
        Self::Stdio {
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            environment: BTreeMap::new(),
        }
    }
}

/// Mirror broker effects; undeclared effects default to false and are never assumed available.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityEffectsConfig {
    pub repo_write: bool,
    pub outside_write: bool,
    pub network: bool,
    pub git_metadata_write: bool,
    pub git_push_or_destructive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_config_defaults_to_unset_for_both_agents() {
        let worker = WorkerConfig::default();
        assert_eq!(worker.claude, None);
        assert_eq!(worker.codex, None);
        assert_eq!(worker.default_depth, 1);
        assert!(!worker.default_read_only);
        assert_eq!(worker.max_depth, u8::MAX);
        assert!(!worker.deny_network);
        assert_eq!(worker.bootstrap_timeout_secs, 600);
    }

    #[test]
    fn worktree_config_defaults_to_a_small_bounded_pool() {
        let worktree = WorktreeConfig::default();
        assert_eq!(worktree.idle_pool_max, 4);
        assert_eq!(worktree.idle_ttl_secs, 3600);
    }

    /// Deny continues to beat allow even when both sides of the conflict
    /// come from operator-added entries rather than the shipped lists --
    /// the underlying CLI mechanism does not care which list an entry came
    /// from, but this pins that the config layer does not accidentally
    /// separate them in a way that would matter.
    #[test]
    fn an_operator_added_deny_entry_beats_an_operator_added_allow_entry() {
        use super::super::super::adapters::AgentAdapter;
        let cfg = CtxConfig {
            sandbox: SandboxConfig {
                enabled: true,
                extra_allow: vec!["Bash(deploy *)".to_string()],
                extra_deny: vec!["Bash(deploy *)".to_string()],
                scrub_subprocess_env: false,
                scrub_worker_secrets: true,
            },
            ..CtxConfig::default()
        };
        let claude = super::super::super::adapters::claude::ClaudeAdapter::new(None);
        let args = claude.default_sandbox_args(
            &cfg.sandbox,
            &Default::default(),
            &[],
            super::super::super::adapters::LaunchMode::Headless,
        );
        let allow_arg = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("allow token");
        let deny_arg = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("deny token");
        assert!(allow_arg.contains("Bash(deploy *)"));
        assert!(
            deny_arg.contains("Bash(deploy *)"),
            "both lists carry the entry; claude's own engine resolves the conflict as deny-wins \
             (verified live for the shipped pair), not this config layer"
        );
    }

    /// A malformed `[policy]` table fails the load loudly rather than
    /// defaulting the whole section to `allow` -- the same "loud rather than
    /// silent" rule `reject_untrusted_keys` follows, applied to a section
    /// where a silent default is a permission grant.
    #[test]
    fn fallback_defaults_are_conservative_and_enabled() {
        let cfg = FallbackConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.order, vec!["claude", "codex"]);
        assert_eq!(cfg.predictive_headroom_pct, 20.0);
        assert_eq!(cfg.min_candidate_headroom_pct, 10.0);
        assert_eq!(cfg.unknown_headroom_pct, 25.0);
        assert_eq!(cfg.small_task_max_tokens, 40_000);
        assert_eq!(cfg.small_task_max_tool_calls, 24);
        // Issue #358.
        assert!(cfg.adaptive_delegation);
        assert_eq!(
            cfg.auto_orchestrator_rollover, None,
            "unset: decided from the roster"
        );
        assert_eq!(cfg.orchestrator_rollover_headroom_pct, None);
        assert_eq!(cfg.rollover_cooldown_secs, 600);
        assert_eq!(cfg.reactive_force_after_secs, 120);
        assert!(cfg.harness.is_empty());
        assert_eq!(cfg.rollover_headroom_pct(), cfg.predictive_headroom_pct);
        assert_eq!(cfg.harness_limits("codex"), HarnessLimits::default());
        assert_eq!(
            cfg.reserve_headroom_pct("codex"),
            cfg.min_candidate_headroom_pct
        );
    }
}
