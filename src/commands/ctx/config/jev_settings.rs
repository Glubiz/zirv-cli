use super::*;

/// Starting decider; failures always fall toward Deterministic, which makes no model calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyDecider {
    #[default]
    Typesafe,
    Helper,
    Deterministic,
}

/// Keep truncated requests large enough for meaningful model decisions; zero would always supply empty input.
pub const MIN_PROXY_REQUEST_MAX_BYTES: usize = 1024;

/// Operator-only proxy controls: repos cannot enable spending or loosen confidence/request bounds (#537).
/// Disabled by default; numeric bounds are validated once at load time, never silently clamped.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub decider: ProxyDecider,
    /// Minimum per-field confidence in `0.0..=1.0`; lower answers use the deterministic baseline.
    pub min_confidence: f32,
    /// Minimum top-to-runner-up probability margin in `0.0..=1.0`, required alongside confidence.
    /// Confidence alone cannot rule out an unstable answer.
    pub min_margin: f32,
    /// Request byte cap before any model call; at least [`MIN_PROXY_REQUEST_MAX_BYTES`].
    pub request_max_bytes: usize,
    pub typesafe: ProxyTypesafeConfig,
    /// One-launch operator overrides from CLI flags; never read from a config file (#537).
    #[serde(skip)]
    pub overrides: crate::commands::ctx::proxy::decision::ProxyOverride,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            decider: ProxyDecider::default(),
            min_confidence: 0.5,
            min_margin: crate::commands::ctx::jev::DEFAULT_MIN_MARGIN,
            request_max_bytes: 16_384,
            typesafe: ProxyTypesafeConfig::default(),
            overrides: Default::default(),
        }
    }
}

/// Primary Jev endpoint; `credential_env` names the API-key variable, never stores its value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyTypesafeConfig {
    pub base_url: String,
    pub credential_env: String,
    /// Pin a Jev release to separate answer variability from model upgrades; `jev-latest` explicitly opts into upgrades.
    pub model: String,
    /// Connect and receive timeout for `/systemone`, validated as at least one second.
    pub timeout_secs: u64,
}

impl Default for ProxyTypesafeConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.typesafe.ai/v1".to_string(),
            credential_env: "TYPESAFE_API_KEY".to_string(),
            model: "jev-1.13.0".to_string(),
            timeout_secs: 10,
        }
    }
}

/// Operator-only advisory gates: repos cannot enable Jev spending (#537).
/// A site needs both its gate and Jev availability; otherwise it keeps the deterministic path.
/// Connection settings are shared with `[proxy.typesafe]`; floating-point floors preclude `Eq` (#803).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevConfig {
    pub memory: bool,
    pub supervisor: bool,
    pub dispatch: bool,
    pub review: bool,
    pub gates: bool,
    pub context: bool,
    pub intake_savings: bool,
    pub review_reuse: bool,
    pub harvest_screen: bool,
    pub admin_dispatch: bool,
    /// May only escalate deterministic allow to ask/deny, never loosen a verdict (#781).
    pub approve: bool,
    /// Opt in to lowering ask to allow; requires `approve`, otherwise no call or verdict change occurs (#781).
    pub approve_allow: bool,
    /// Refine unrouted workflow intent; classify also adds domain tags when an execution profile exists (#782).
    pub classify: bool,
    /// Jev keep/drop scoring for handoff items (#783).
    pub handoff_select: bool,
    /// Append Jev-ranked items to compaction focus text (#798).
    pub compaction_select: bool,
    /// Screen untrusted inputs for prompt injection (#784).
    pub inject_screen: bool,
    /// Gate automatic compact/restart/mail/Stop injections with Jev (#785).
    pub inject: bool,
    /// Check unverified completion claims at Stop (#786).
    pub stop_verify: bool,
    /// Opt-in metadata-only test-necessity check before the deterministic missing-tests block.
    /// Only a decisive not-owed answer skips the block without persisting it; all other outcomes retain the block.
    pub missing_tests: bool,
    /// Refine configured first-turn headless effort from numeric metadata; unavailable or indecisive answers use the classifier.
    /// Persist the choice per session so resume never re-asks or changes effort mid-conversation.
    pub launch_effort: bool,
    /// After repeated identical tool failures, asks one retry/stop/change-approach question per streak; a decisive stop or change adds one advisory line, retry or any error adds nothing (#836).
    pub retry: bool,
    /// Shared exact-request cache lifetime; zero disables reads and writes, and the model is part of the key.
    /// Caching stabilizes decisions near confidence floors because uncached Jev confidence varies between calls.
    pub cache_ttl_secs: u64,
    pub floors: JevFloorsConfig,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            memory: false,
            supervisor: false,
            dispatch: false,
            review: false,
            gates: false,
            context: false,
            intake_savings: false,
            review_reuse: false,
            harvest_screen: false,
            admin_dispatch: false,
            approve: false,
            approve_allow: false,
            classify: false,
            handoff_select: false,
            compaction_select: false,
            inject_screen: false,
            inject: false,
            stop_verify: false,
            missing_tests: false,
            launch_effort: false,
            retry: false,
            cache_ttl_secs: 604_800,
            floors: JevFloorsConfig::default(),
        }
    }
}

/// Optional per-site floors in `[0.0, 1.0]`, validated at load time; each unset value keeps its compiled default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevSiteFloor {
    pub min_confidence: Option<f32>,
    pub min_margin: Option<f32>,
}

/// Operator-only advisory floor overrides; repos cannot loosen or tighten acted-on decisions (#803).
/// Unset values retain compiled defaults; safety and verification sites always retain their untunable compiled floors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevFloorsConfig {
    pub memory: JevSiteFloor,
    pub context: JevSiteFloor,
    pub harvest_screen: JevSiteFloor,
    pub handoff_select: JevSiteFloor,
    pub compaction_select: JevSiteFloor,
    pub dispatch: JevSiteFloor,
    pub launch_effort: JevSiteFloor,
    pub classify: JevSiteFloor,
    pub inject: JevSiteFloor,
}

/// Operator-only, opt-in Claude headless controls; interactive wrap/chat/dashboard paths never read them (#788).
/// Repos cannot change billing, effort or tool/memory scope; unset keys leave launches unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadlessEffortConfig {
    pub trivial: Option<String>,
    pub bounded: Option<String>,
    /// Also covers Architectural: the text-only classifier cannot produce that class, so no separate key is needed.
    pub substantial: Option<String>,
}

/// Operator-only headless controls with the scope and trust constraints of [`HeadlessEffortConfig`] (#788).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadlessConfig {
    /// Optional `5m`/`1h` cache TTL; existing `CLAUDE_CODE_PROMPT_CACHE_TTL`,
    /// `FORCE_PROMPT_CACHING_5M` or `ENABLE_PROMPT_CACHING_1H` environment values take precedence.
    pub prompt_cache_ttl: Option<String>,
    /// Optional per-class effort; existing `CLAUDE_CODE_EFFORT_LEVEL` or `--effort` wins.
    /// Classification is pure unless `[jev] launch_effort` enables first-launch metadata refinement.
    pub effort: HeadlessEffortConfig,
    /// Disable auto-memory and bundled skills in the headless settings layer.
    pub lean: bool,
    /// Additional headless `--disallowedTools` entries; empty by default.
    pub disallowed_tools: Vec<String>,
}

/// Operator-only review-model overrides; unset values use the adapter ladder below the orchestrator model.
/// Repos cannot set these overrides, but may shift ladder defaults through the visibly announced `chat.model`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #537 seam: `[proxy]` defaults match the spec's table exactly --
    /// disabled, `typesafe` first, floor 0.5, 16 KiB request cap, the
    /// documented Jev endpoint/credential-env/model/timeout.
    #[test]
    fn proxy_config_defaults_match_the_spec() {
        let cfg = ProxyConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.decider, ProxyDecider::Typesafe);
        assert_eq!(cfg.min_confidence, 0.5);
        assert_eq!(
            cfg.min_margin,
            crate::commands::ctx::jev::DEFAULT_MIN_MARGIN
        );
        assert_eq!(cfg.request_max_bytes, 16_384);
        assert_eq!(cfg.typesafe.base_url, "https://api.typesafe.ai/v1");
        assert_eq!(cfg.typesafe.credential_env, "TYPESAFE_API_KEY");
        assert_eq!(cfg.typesafe.model, "jev-1.13.0");
        assert_eq!(cfg.typesafe.timeout_secs, 10);
    }

    /// Issue #537 seam extraction: every `[jev]` advisory-site key defaults
    /// to off, so a Jev-backed decision path never activates until an
    /// operator opts a specific site in.
    #[test]
    fn jev_config_defaults_to_all_sites_off() {
        let cfg = JevConfig::default();
        assert!(!cfg.memory);
        assert!(!cfg.supervisor);
        assert!(!cfg.dispatch);
        assert!(!cfg.review);
        assert!(!cfg.gates);
        assert!(!cfg.context);
        assert!(!cfg.intake_savings);
        assert!(!cfg.review_reuse);
        assert!(!cfg.harvest_screen);
        assert!(!cfg.admin_dispatch);
        assert!(!cfg.approve);
        assert!(!cfg.approve_allow);
        assert!(!cfg.classify);
        assert!(!cfg.handoff_select);
        assert!(!cfg.compaction_select);
        assert!(!cfg.inject_screen);
        assert!(!cfg.inject);
        assert!(!cfg.stop_verify);
        assert!(!cfg.launch_effort);
        assert_eq!(cfg.cache_ttl_secs, 604_800);
    }

    #[test]
    fn review_config_defaults_to_unset_for_both_agents() {
        let review = ReviewConfig::default();
        assert_eq!(review.claude, None);
        assert_eq!(review.codex, None);
    }
}
