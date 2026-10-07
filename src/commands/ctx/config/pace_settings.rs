use super::*;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub window: usize,
    pub min_turns: usize,
    /// Absolute pressure floor wins over the capacity ratio; unset derives it from resolved capacity.
    pub token_floor: Option<u64>,
    /// Absolute pressure ceiling wins over the capacity ratio.
    pub token_ceiling: Option<u64>,
    /// Capacity fraction used when no absolute floor is set (#155).
    pub token_floor_ratio: f64,
    /// Capacity fraction used when no absolute ceiling is set.
    pub token_ceiling_ratio: f64,
    /// Operator-known context capacity overrides the adapter estimate; unset defers to the adapter.
    pub model_context_tokens: Option<u64>,
    pub weight_tool_failure: f64,
    pub weight_repetition: f64,
    pub weight_marker: f64,
    /// Weight consecutive identical normalized errors; zero disables the signal (#763).
    /// The default first crossing contributes only an advise score in an otherwise healthy session, then ramps upward.
    pub same_error_weight: f64,
    pub repetition_threshold: usize,
    /// Consecutive identical normalized errors required to trip the ramped repetition signal.
    pub same_error_threshold: usize,
    pub advise_at: u32,
    pub compact_at: u32,
    pub restart_at: u32,
    pub marker: String,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            window: 10,
            min_turns: 10,
            token_floor: None,
            token_ceiling: None,
            token_floor_ratio: 0.5,
            token_ceiling_ratio: 0.8,
            model_context_tokens: None,
            weight_tool_failure: 40.0,
            weight_repetition: 30.0,
            weight_marker: 30.0,
            same_error_weight: 120.0,
            repetition_threshold: 3,
            same_error_threshold: 3,
            advise_at: 40,
            compact_at: 60,
            restart_at: 80,
            marker: DEFAULT_MARKER.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WrapConfig {
    pub debounce_ms: u64,
    pub inject_timeout_ms: u64,
}

impl Default for WrapConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 3000,
            inject_timeout_ms: 20_000,
        }
    }
}

/// Declared order is strictness: repos may only tighten Allow to Advise to Deny (#358, #328, #334).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrchestratorWrites {
    /// Allow silently but log every write for status counts.
    Allow,
    /// Allow with a rate-limited advisory; log every occurrence.
    #[default]
    Advise,
    /// Refuse the write with guidance to delegate it (#328, #334).
    Deny,
}

impl OrchestratorWrites {
    pub fn label(self) -> &'static str {
        match self {
            OrchestratorWrites::Allow => "allow",
            OrchestratorWrites::Advise => "advise",
            OrchestratorWrites::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuperviseConfig {
    pub max_restarts: u32,
    pub poll_ms: u64,
    pub interval_secs: u64,
    pub max_cycle_secs: u64,
    pub max_failures: u32,
    pub backoff_base_secs: u64,
    pub on_failure: Option<String>,
    /// Nudge restarts use a separate cap because they are not rot; excess mail remains unread and visible in inbox.
    /// Repos may tune interruption counts because this selects no binary, command or model.
    pub max_nudges: u32,
    /// Operator-only machine-wide heavy-command concurrency; hold each permit for the child lifetime (#155).
    /// One operation is the safe default against host overload (#133).
    /// The deprecated `max_heavy_workers` alias remains accepted and repo-forbidden; the canonical key wins.
    pub max_heavy_operations: usize,
    /// Operator-only machine-wide lifetime writer cap, independent of heavy-command permits (#267, #338).
    /// Zero leaves only per-checkout exclusivity; never allow two writers on one checkout, and read-only workers never count.
    pub max_writers: usize,
    /// Add heavy-command patterns without ever removing built-ins; repos may add restrictions only (#133).
    pub heavy_command_patterns: Vec<String>,
    /// Operator-only idle progress fuse across PTY output, transcript growth and mail; repos cannot defeat detection (#310).
    pub idle_no_tool_secs: u64,
    /// Operator-only progress fuse inside a tool call, allowing more time than idle thinking (#310).
    pub in_tool_secs: u64,
    /// Operator-only grace after the stall nudge before termination; repos cannot defeat the fuse (#310).
    pub stall_grace_secs: u64,
    /// Operator-only compaction stall fuse, sized for slow multi-minute compactions; repos cannot suppress detection (#379).
    pub compact_stall_secs: u64,
    /// Operator-only headless compaction bound, covering child exit and boundary verification.
    /// A full model turn can take minutes and writes no transcript until completion, so growth cannot prove liveness.
    /// Repos may neither shorten it into premature restarts nor lengthen it to hide a hung compaction.
    pub compact_timeout_ms: u64,
    /// Operator-only same-class unplanned restart limit breaks cross-process respawn loops; repos cannot raise it (#310).
    pub chain_max_restarts: u32,
    /// Operator-only maximum gap linking respawns into the restart chain (#310).
    pub chain_max_gap_secs: u64,
    /// Default Advise allows writes with a rate-limited advisory and durable count (#358, #328, #334).
    /// Repos may only tighten the posture toward Deny.
    pub orchestrator_writes: OrchestratorWrites,
    /// Self-paced loop delay ceiling; an explicit `--interval` bypasses it (#311).
    /// Repos may only lower it, requesting more frequent supervision rather than longer quiet periods.
    pub loop_backoff_ceiling_secs: u64,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        Self {
            max_restarts: 2,
            poll_ms: 2000,
            interval_secs: 900,
            max_cycle_secs: 3600,
            max_failures: 5,
            backoff_base_secs: 60,
            on_failure: None,
            max_nudges: 3,
            max_heavy_operations: 1,
            max_writers: 0,
            heavy_command_patterns: Vec::new(),
            idle_no_tool_secs: 450,
            in_tool_secs: 1200,
            stall_grace_secs: 120,
            compact_stall_secs: 600,
            compact_timeout_ms: 600_000,
            chain_max_restarts: 3,
            chain_max_gap_secs: 300,
            orchestrator_writes: OrchestratorWrites::Advise,
            loop_backoff_ceiling_secs: 900,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Repo-relative exclusions for advisory reuse scanning; empty covers the checkout (#406).
    /// Repo exclusions reduce advice but never widen authority because the probe cannot deny writes.
    pub reuse_exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    /// Unset delegates distiller choice to the adapter: Claude has a cheap default, Codex has no verified one.
    pub model: Option<String>,
    /// One trailing-item limit shared by user messages, assistant text and tool errors.
    pub tail_items: usize,
    /// Bound the distiller before structural fallback: an unbounded wait in the wrap pump freezes the terminal.
    pub timeout_secs: u64,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            model: None,
            tail_items: 5,
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaceConfig {
    pub enabled: bool,
    /// A supervised window is kept at or below this percentage.
    pub max_percent: f64,
    /// Operator-only reading freshness: extending it keeps stale data binding, while shortening it can bypass the gate.
    pub collector_max_age_secs: u64,
    /// Operator-only estimator selection; a repo choosing both source and budget would control the whole reading.
    pub estimator: bool,
    /// Zero disables this estimator window because no documented allowance supports a default; operator-only.
    pub five_hour_budget_tokens: u64,
    /// Operator-only budget for the same estimator-authority constraint as `five_hour_budget_tokens`.
    pub seven_day_budget_tokens: u64,
    /// Operator-only cache-read accounting; its large share can change the pacing verdict.
    pub count_cache_reads: bool,
    pub jitter_secs: u64,
    /// Used when a window's `resets_at` is unknown.
    pub fallback_delay_secs: u64,
    /// Slack above window length tolerates a slightly incorrect reset timestamp.
    pub wait_slack_secs: u64,
    /// Optional wait cap; otherwise scale to the exhausted window plus slack to avoid spending before reset.
    pub max_wait_secs: Option<u64>,
    /// Delay cycles linearly across the remaining window above this floor; at or above max-percent disables soft throttling.
    pub soft_percent: f64,
    /// Poll only when passive collector data is stale at a gate.
    pub poll_enabled: bool,
    /// Per-provider floor between poll attempts, shared across processes.
    pub poll_min_interval_secs: u64,
    /// Operator-declared overage coverage bypasses throttle and pause for that harness.
    pub use_credits: UseCreditsConfig,
    /// Apply a bounded delay with no collector or estimator: never fail open at full speed.
    /// Keep it small for one-shot calls while slowing unobserved automated spending loops.
    pub blind_delay_secs: u64,
    /// Operator-only soft gate for new spend, stricter than pacing ongoing work (#155).
    /// Never drive restarts from cost: that discards warm caches; rot and score never read pacing fields.
    pub spawn_soft_pct: f64,
    /// Operator-only hard spawn ceiling; refusal requires an authorized force override to bypass (#155).
    pub spawn_hard_pct: f64,
    /// Operator-only default objective budget; unset is unbounded (#285).
    /// Exhaustion changes objective status and wrap-up guidance, never kills the run.
    pub run_budget_tokens: Option<u64>,
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_percent: 99.0,
            collector_max_age_secs: 900,
            estimator: true,
            five_hour_budget_tokens: 0,
            seven_day_budget_tokens: 0,
            count_cache_reads: false,
            jitter_secs: 30,
            fallback_delay_secs: 900,
            wait_slack_secs: 3600,
            max_wait_secs: None,
            soft_percent: 80.0,
            poll_enabled: true,
            poll_min_interval_secs: 60,
            use_credits: UseCreditsConfig::default(),
            blind_delay_secs: 60,
            spawn_soft_pct: 80.0,
            spawn_hard_pct: 95.0,
            run_budget_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UseCreditsConfig {
    pub claude: bool,
    pub codex: bool,
}

impl UseCreditsConfig {
    /// Map agent-keyed config to providers; unknown providers must still gate.
    pub fn for_provider(&self, provider: &str) -> bool {
        match provider {
            "anthropic" => self.claude,
            "openai" => self.codex,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptimizeConfig {
    /// Whether the Stop hook may queue an "optimize recommended" entry.
    pub enabled: bool,
    pub sessions_sampled: usize,
    pub max_surface_bytes: usize,
    /// Empty reuses handoff model resolution so cheap-model selection stays adapter-specific.
    pub model: String,
    pub recommend_tool_failure_rate: f64,
    pub recommend_corrections: usize,
    pub recommend_cooldown_secs: u64,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sessions_sampled: 10,
            max_surface_bytes: 200_000,
            model: String::new(),
            recommend_tool_failure_rate: 0.25,
            recommend_corrections: 3,
            recommend_cooldown_secs: 86_400,
        }
    }
}

/// Nudge stale verification after session edits; repos may only disable it or lower the nudge cap (#309).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyOnStopConfig {
    pub enabled: bool,
    pub max_nudges: u32,
}

impl Default for VerifyOnStopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_nudges: 2,
        }
    }
}

/// Block a headless Worker/Single once for source edits without test changes; repos may disable, never enable it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MissingTestsGateConfig {
    pub enabled: bool,
}

impl Default for MissingTestsGateConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Claude SubagentStop contract gate fails open and blocks at most once; repos may disable, never enable it (#774).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentStopGateConfig {
    pub enabled: bool,
}

impl Default for SubagentStopGateConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Record request limits, advise at the first edit, and block an unrequested closing-report fix once per prompt.
/// Repos may disable, never enable it; disabled means no records, checkpoints or Stop blocks.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScopeGuardConfig {
    pub enabled: bool,
}

impl Default for ScopeGuardConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Opt-in PreToolUse guard that denies a scripted rewrite of a tracked file once per command.
/// Off by default; repos may only disable it.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditGuardConfig {
    pub enabled: bool,
}

/// Opt-in local compiler diagnostics after edits; emit only bounded findings new since the session baseline (#308).
/// Off by default because it spawns a process; repos may only disable, lower the count or shorten the timeout.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    pub max_diagnostics: u32,
    pub timeout_secs: u64,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_diagnostics: 10,
            timeout_secs: 120,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let cfg = ScoreConfig::default();
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.min_turns, 10);
        assert_eq!(
            cfg.token_floor, None,
            "absolute overrides are unset by default -- see rot::token_gates"
        );
        assert_eq!(cfg.token_ceiling, None);
        assert_eq!(cfg.token_floor_ratio, 0.5);
        assert_eq!(cfg.token_ceiling_ratio, 0.8);
        assert_eq!(cfg.model_context_tokens, None);
        assert_eq!(cfg.advise_at, 40);
        assert_eq!(cfg.compact_at, 60);
        assert_eq!(cfg.restart_at, 80);
        assert_eq!(cfg.marker, "[zirv]");
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(
            cfg.weight_tool_failure + cfg.weight_repetition + cfg.weight_marker,
            100.0,
            "weights must sum to 100 so an all-signals session can reach restart"
        );
        assert_eq!(WrapConfig::default().debounce_ms, 3000);
        assert_eq!(SuperviseConfig::default().max_restarts, 2);
        assert_eq!(SuperviseConfig::default().max_nudges, 3);
        assert_eq!(
            SuperviseConfig::default().max_heavy_operations,
            1,
            "issue #133: a single heavy operation at a time is the safe default"
        );
        assert_eq!(
            SuperviseConfig::default().max_writers,
            0,
            "issue #338: 0 means no machine-wide cap, with per-tree exclusivity only"
        );
        assert_eq!(
            SuperviseConfig::default().heavy_command_patterns,
            Vec::<String>::new(),
            "the built-in set is baked into permit::is_heavy, not duplicated here"
        );
        assert_eq!(
            SuperviseConfig::default().idle_no_tool_secs,
            450,
            "issue #310: mirrors the Hermes reference's own _STALE_IDLE_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().in_tool_secs,
            1200,
            "issue #310: mirrors the Hermes reference's own _STALE_IN_TOOL_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().stall_grace_secs,
            120,
            "issue #310: mirrors the Hermes reference's own _STALL_GRACE_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().compact_stall_secs,
            600,
            "issue #379: roughly double the slowest compaction actually observed"
        );
        assert_eq!(
            SuperviseConfig::default().chain_max_restarts,
            3,
            "issue #310: mirrors the Hermes reference's own DEFAULT_MAX_RESTARTS"
        );
        assert_eq!(
            SuperviseConfig::default().loop_backoff_ceiling_secs,
            900,
            "issue #311: mirrors Hermes's own DEFAULT_SELF_PACED_CEILING_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().chain_max_gap_secs,
            300,
            "issue #310: mirrors the Hermes reference's own DEFAULT_MAX_GAP_SECONDS"
        );
        assert_eq!(
            HandoffConfig::default().model,
            None,
            "per-adapter resolution now lives in resolve_distiller_model, not a hardcoded default"
        );
        assert_eq!(HandoffConfig::default().tail_items, 5);
        assert_eq!(HandoffConfig::default().timeout_secs, 30);
    }

    #[test]
    fn pacing_defaults_match_the_spec() {
        let pace = PaceConfig::default();
        assert!(pace.enabled, "pacing is on by default");
        assert_eq!(pace.max_percent, 99.0);
        assert_eq!(pace.collector_max_age_secs, 900);
        assert!(pace.estimator);
        assert_eq!(
            (pace.five_hour_budget_tokens, pace.seven_day_budget_tokens),
            (0, 0),
            "no invented budget: the estimator stays quiet until an operator sets one"
        );
        assert!(!pace.count_cache_reads);
        assert_eq!(pace.jitter_secs, 30);
        assert_eq!(pace.fallback_delay_secs, 900);
        assert_eq!(pace.wait_slack_secs, 3600);
        assert_eq!(
            pace.max_wait_secs, None,
            "no global cap by default: the cap is scaled to the window that tripped"
        );
    }

    #[test]
    fn pace_gains_soft_and_poll_and_use_credits_defaults() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.soft_percent, 80.0);
        assert!(cfg.poll_enabled);
        assert_eq!(cfg.poll_min_interval_secs, 60);
        assert!(!cfg.use_credits.claude);
        assert!(!cfg.use_credits.codex);
    }

    #[test]
    fn pace_gains_spawn_soft_and_hard_pct_defaults() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.spawn_soft_pct, 80.0);
        assert_eq!(cfg.spawn_hard_pct, 95.0);
    }

    #[test]
    fn pace_run_budget_tokens_defaults_to_unset() {
        assert_eq!(PaceConfig::default().run_budget_tokens, None);
    }

    #[test]
    fn use_credits_maps_providers_to_agent_flags() {
        let uc = UseCreditsConfig {
            claude: true,
            codex: false,
        };
        assert!(uc.for_provider("anthropic"));
        assert!(!uc.for_provider("openai"));
        assert!(
            !uc.for_provider("something-else"),
            "unknown provider: gate stays on"
        );
    }

    /// Serde round-trips through the documented lowercase strings, not Rust's
    /// own `Debug`/variant-name casing.
    #[test]
    fn orchestrator_writes_serializes_as_lowercase_strings() {
        for (value, text) in [
            (OrchestratorWrites::Allow, "\"allow\""),
            (OrchestratorWrites::Advise, "\"advise\""),
            (OrchestratorWrites::Deny, "\"deny\""),
        ] {
            assert_eq!(serde_json::to_string(&value).expect("serialize"), text);
            let parsed: OrchestratorWrites = serde_json::from_str(text).expect("deserialize");
            assert_eq!(parsed, value);
        }
    }

    /// Q1: default-on, unlike `diagnostics` -- an operator who never touches
    /// `missing_tests_gate` still gets the check.
    #[test]
    fn missing_tests_gate_defaults_on() {
        assert!(MissingTestsGateConfig::default().enabled);
    }

    /// Issue #774: default-on, the same as `missing_tests_gate` -- an
    /// operator who never touches `subagent_stop_gate` still gets the check.
    #[test]
    fn subagent_stop_gate_defaults_on() {
        assert!(SubagentStopGateConfig::default().enabled);
    }

    /// Default-on, the same as `missing_tests_gate`/`subagent_stop_gate` --
    /// an operator who never touches `scope_guard` still gets the guard.
    #[test]
    fn scope_guard_defaults_on() {
        assert!(ScopeGuardConfig::default().enabled);
    }

    #[test]
    fn optimize_defaults_are_conservative() {
        let optimize = OptimizeConfig::default();
        assert!(optimize.enabled, "the hook recommendation is on by default");
        assert_eq!(optimize.sessions_sampled, 10);
        assert_eq!(optimize.max_surface_bytes, 200_000);
        assert_eq!(
            optimize.model, "",
            "empty means reuse the handoff model rather than inventing a second default"
        );
        assert_eq!(optimize.recommend_tool_failure_rate, 0.25);
        assert_eq!(optimize.recommend_corrections, 3);
        assert_eq!(optimize.recommend_cooldown_secs, 86_400);
    }
}
