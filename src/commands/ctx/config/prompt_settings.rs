use super::*;

/// Operator-only orchestrator orientation tiers with pinned byte budgets; repos may never set them (#427).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptVerbosity {
    Minimal,
    Standard,
    #[default]
    Verbose,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub enabled: bool,
    /// Whether `<repo>/.zirv/system-prompt.md` is read at all.
    pub repo_layer: bool,
    /// Operator-only orientation verbosity; repos cannot restore a larger tier the operator disabled (#427).
    pub verbosity: PromptVerbosity,
    /// Cap on the repo layer only: untrusted text does not get to be long.
    pub max_repo_bytes: usize,
    /// Operator-only harness roster switch; repos cannot hide delegation choices from the session.
    pub harnesses: bool,
    /// Repos may only disable the standing skill index; skills remain explicitly loadable either way.
    pub skill_index: bool,
    /// Classify only the first prompt without network access and add discipline for substantial work (#753).
    /// Repos may disable the note but never force it on.
    pub intake_discipline: bool,
    /// Operator-only filtering of skill families lacking repo signals; hidden skills remain loadable (#755).
    /// Repos cannot disable filtering to widen their standing advertisements.
    pub skill_index_repo_filter: bool,
    /// Operator-only Codex orchestrator layer; repos cannot re-enable it, and the switch never affects Claude (#167).
    pub codex_orchestrator: bool,
    /// Copied from `[supervise]` for prompt composition; serde skips it so the wrong `[prompt]` key hard-errors.
    #[serde(skip)]
    pub orchestrator_writes: OrchestratorWrites,
    /// Copied from `[edit_guard]`: the worker "use the Edit tool" rule rides along with the guard; serde skips it.
    #[serde(skip)]
    pub edit_guard: bool,
    /// Set by `compile` when the host already lists the skills natively, so the prompt carries only the pointer; serde skips it, so it is no config key.
    #[serde(skip)]
    pub skill_index_native: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repo_layer: true,
            verbosity: PromptVerbosity::Verbose,
            max_repo_bytes: 4096,
            harnesses: true,
            skill_index: true,
            intake_discipline: true,
            skill_index_repo_filter: true,
            codex_orchestrator: true,
            orchestrator_writes: OrchestratorWrites::Advise,
            edit_guard: false,
            skill_index_native: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Operator-only common-context byte cap; untrusted repo text cannot raise its own limit (#44).
    pub max_common_bytes: usize,
    /// Separate byte cap for harness additions because each canonical file is truncated independently.
    pub max_harness_bytes: usize,
    /// Operator-only roster byte cap enforced during composition, not merely reported by context status (#46).
    pub max_harness_roster_bytes: usize,
    /// Skip canonical injection only when the managed native instruction file hash proves identical content (#155).
    /// Repos may only disable deduplication: injecting more context is the safe direction.
    pub dedupe_native: bool,
    /// Operator-only bound on quadratic lint comparisons over untrusted prose (#275).
    /// Exhaustion returns a partial degraded report rather than hanging or failing lint.
    pub lint_max_pairs: usize,
    /// Operator-only aggregate native-instruction cap prevents many small files bypassing per-file limits (#538).
    /// Apply after per-file caps in stable root-first, then ancestor-depth order.
    pub instructions_max_bytes: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_common_bytes: 4096,
            max_harness_bytes: 4096,
            max_harness_roster_bytes: 4096,
            dedupe_native: true,
            lint_max_pairs: 20_000,
            instructions_max_bytes: 32 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MailConfig {
    pub enabled: bool,
    /// Truncate oversized stored message bodies instead of rejecting them.
    pub max_message_bytes: usize,
    /// Byte cap on mail surfaced to a session in one delivery.
    pub max_delivered_bytes: usize,
    /// Prune the oldest unread messages beyond this count; never touch `read/` entries.
    pub keep: usize,
    /// Deliver mail addressed to a Claude session from its `PostToolUse` hook, mid-turn.
    pub mid_turn: bool,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_message_bytes: 4096,
            max_delivered_bytes: 4096,
            keep: 50,
            mid_turn: false,
        }
    }
}

/// Operator-only deploy tier; the repo-controlled minimum may only increase strictness.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowDeployConfig {
    pub tier: crate::commands::workflow::deploy::DeployTier,
    pub minimum_tier: Option<crate::commands::workflow::deploy::DeployTier>,
}

impl Default for WorkflowDeployConfig {
    fn default() -> Self {
        Self {
            tier: crate::commands::workflow::deploy::DeployTier::Development,
            minimum_tier: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaintainDetectorMode {
    #[default]
    ExitNonzero,
    LineCount,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintainDetectorConfig {
    pub command: String,
    pub mode: MaintainDetectorMode,
    pub threshold: u64,
}

impl Default for MaintainDetectorConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            mode: MaintainDetectorMode::ExitNonzero,
            threshold: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowMaintainConfig {
    pub timeout_secs: u64,
    pub detectors: std::collections::BTreeMap<String, MaintainDetectorConfig>,
}

impl Default for WorkflowMaintainConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 60,
            detectors: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReportConfig {
    /// Maintenance incident destination; ordinary `zirv report` keeps its product default.
    pub repository: Option<String>,
}

/// Operator-only search output bound prevents history retrieval flooding the calling session (#315).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Hard byte cap on one rendered search window.
    pub max_output_bytes: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: 2048,
        }
    }
}

/// Operator-only output shaping: repos may neither hide their output behind summaries nor bypass compaction (#326).
/// Repos may only lower `diff_max_bytes`, keeping diffs bounded (#412).
/// Structured filters are home-config-only, and bundled-filter selection is forbidden to repos in both directions (#417).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Compact large Bash results before the model sees them; explicit `run --compact` is unaffected.
    pub compact: bool,
    /// Known-output byte threshold avoids storage and retrieval overhead for already short results.
    pub compact_min_bytes: usize,
    /// Higher threshold for unknown producers because a generic head/tail summary cannot guarantee relevant content.
    pub compact_generic_min_bytes: usize,
    /// Operator-only additions to programs never compacted; built-in verbatim protection cannot be removed.
    pub verbatim: Vec<String>,
    /// Hard summary cap; if mandatory failure content cannot fit, preserve the original rather than hide failures.
    /// Load validation rejects values below [`MIN_MAX_SUMMARY_BYTES`].
    pub max_summary_bytes: usize,
    /// Replace oversized diffs with bounded file listings, never partial hunks a model might edit against (#412).
    /// Repos may only lower the threshold.
    pub diff_max_bytes: usize,
    /// Operator-only shape-aware search compaction; repos cannot change their output treatment in either direction (#414).
    /// The original remains retrievable through `zirv ctx output show`.
    pub compact_search: bool,
    /// Home-only Generic-output rules, applied before head/tail summarization; never affect other scopes (#417).
    /// First matching rule wins; bundled defaults follow operator rules and skip names already supplied.
    pub filter: Vec<OutputFilterRule>,
    /// Operator-only bundled-filter switch; repos cannot change their output shaping by enabling or disabling defaults.
    pub filter_defaults: bool,
}

/// Output-filter contract: name and command selector are mandatory; absent optional stages do nothing (#417).
/// Fixed order: whole-output replacement short-circuits, then strip, keep, character truncation, and line limit.
/// Never split a multibyte character; line truncation reports omitted count and original-output retrieval.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputFilterRule {
    /// Required rule identity makes load errors attributable.
    pub name: String,
    /// Regex over the joined command line, validated to compile and anchor every top-level alternative.
    /// Unanchored alternatives could select an unrelated program.
    pub match_command: String,
    /// Drop lines matching any pattern before keep filtering; empty drops nothing.
    #[serde(default)]
    pub strip_lines: Vec<String>,
    /// Keep lines matching any pattern after stripping; empty preserves all survivors.
    #[serde(default)]
    pub keep_lines: Vec<String>,
    /// Optional character limit; never slice through a multibyte codepoint.
    #[serde(default)]
    pub truncate_line_at: Option<usize>,
    /// Optional surviving-line limit; append omitted count and retrieval guidance when truncated.
    #[serde(default)]
    pub max_lines: Option<usize>,
    /// Replace only when the match spans the entire original output; skip every other stage on success.
    #[serde(default)]
    pub match_output: Option<MatchOutput>,
}

/// Whole-output replacement contract for `[[output.filter]]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOutput {
    /// Must compile; explicit full-span matching is stronger than requiring regex anchors.
    pub pattern: String,
    /// Literal replacement with no capture-group interpolation.
    pub replace: String,
}

/// Minimum space for header, failure and retrieval lines; smaller settings hard-error rather than silently clamp.
pub const MIN_MAX_SUMMARY_BYTES: usize = 512;

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            compact: true,
            compact_min_bytes: 4096,
            compact_generic_min_bytes: 16384,
            verbatim: Vec::new(),
            max_summary_bytes: 4096,
            diff_max_bytes: 65536,
            compact_search: true,
            filter: Vec::new(),
            filter_defaults: true,
        }
    }
}

/// Operator-only pricing source and freshness: repos cannot choose prices or conceal staleness (#264).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriceConfig {
    /// Mark older price tables as approximate with their date; never present stale pricing as exact.
    pub stale_after_days: u64,
    /// Optional non-default price-table path merged over built-ins; unset uses normal resolution.
    pub table_path: Option<String>,
}

impl Default for PriceConfig {
    fn default() -> Self {
        Self {
            stale_after_days: 90,
            table_path: None,
        }
    }
}

/// Operator-only account model discovery, fetched-price refresh and family pins.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// Read account-local Codex cache and observed Claude transcript model ids.
    pub discovery: bool,
    /// Allow `ctx models refresh` (explicit or the automatic background run) to fetch public price catalogues.
    pub price_fetch: bool,
    /// Family pins keyed as `vendor.family`, for example `openai.sol`.
    pub pin: BTreeMap<String, String>,
    /// Model ids never resolved from a tier or rung (an explicit pin still wins).
    pub avoid: Vec<String>,
    /// Also avoid models whose recorded task success is significantly worse than a same-tier peer's.
    pub auto_avoid: bool,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            discovery: true,
            price_fetch: true,
            pin: BTreeMap::new(),
            avoid: Vec::new(),
            auto_avoid: false,
        }
    }
}

impl ModelsConfig {
    /// Operator file plus `ZIRV_CTX_MODELS_*` only; `models.*` is repo-forbidden, so this is the whole truth.
    pub(crate) fn load_operator_only(env: EnvLookup<'_>) -> CtxResult<Self> {
        load_operator_section(env, "models")
    }
}

/// Operator-only evidence-driven model routing: the new-model promotion gate, automatic probes
/// and the router. `enabled = false` restores the pre-routing behaviour exactly.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    /// Master switch for the promotion gate, the router and canaries.
    pub enabled: bool,
    /// Hold a new family version on probation until evidence says it is not worse; false adopts it at once.
    pub hold_new_models: bool,
    /// Run automatic synthetic probes.
    pub probe: bool,
    /// Hours between probe runs; clamped to at least 24 wherever it is used.
    pub probe_interval_hours: u64,
    /// Spend cap per probe run in USD, in (0, 100].
    pub probe_max_usd: f64,
    /// A harness is probed only with at least this much headroom in its binding usage window.
    pub probe_min_headroom_pct: f64,
    /// Probe harnesses that carry an `[endpoint.<h>]` override (metered spend).
    pub probe_metered: bool,
    /// Non-inferiority margin on the 0-1 quality scale, in [0, 0.5].
    pub tolerance: f64,
    /// Share of low-risk worker delegations sent to a probation candidate, at most 50.
    pub canary_pct: u8,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hold_new_models: true,
            probe: true,
            probe_interval_hours: 24,
            probe_max_usd: 2.0,
            probe_min_headroom_pct: 50.0,
            probe_metered: false,
            tolerance: 0.05,
            canary_pct: 5,
        }
    }
}

impl RoutingConfig {
    /// Operator file plus `ZIRV_CTX_ROUTING_*` only; `routing.*` is repo-forbidden, so this is the whole truth.
    pub(crate) fn load_operator_only(env: EnvLookup<'_>) -> CtxResult<Self> {
        load_operator_section(env, "routing")
    }

    /// The probe interval in seconds, never below the 24 h floor.
    #[allow(dead_code)] // read by the probe scheduler, which lands after the evidence store
    pub fn probe_interval_secs(&self) -> u64 {
        self.probe_interval_hours.max(24).saturating_mul(3600)
    }
}

/// Ignorable compaction advice requires both reclaim and context thresholds (#312).
/// Repos may only raise them to quiet advice; these do not control rot supervision.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactAdvisoryConfig {
    /// Minimum stale tool-result volume required before compaction advice.
    pub min_reclaim_tokens: u64,
    /// Context-capacity fraction also required before compaction advice.
    pub window_fraction: f64,
}

impl Default for CompactAdvisoryConfig {
    fn default() -> Self {
        Self {
            min_reclaim_tokens: 4096,
            window_fraction: 0.6,
        }
    }
}

/// Operator-only workflow controls except `deploy.minimum_tier`, which repos may only tighten.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowConfig {
    /// Disabled repo checks still appear as skipped so operators can inspect requested execution.
    pub repo_checks_enabled: bool,
    /// Enable repo skill loading; repo manifests may add ids but never replace trusted ones.
    pub repo_skills_enabled: bool,
    /// Opt-in repo agents may propose roles but never replace built-in or operator ids.
    pub repo_agents_enabled: bool,
    /// Opt-in repo workflow packs may neither replace trusted ids nor widen authority (#542).
    pub repo_workflows_enabled: bool,
    pub deploy: WorkflowDeployConfig,
    pub maintain: WorkflowMaintainConfig,
    /// Operator-controlled local workflow telemetry; repo scripts cannot authorize it.
    pub telemetry_enabled: bool,
    pub telemetry_max_events: usize,
    pub telemetry_retention_days: u64,
    /// Operator-only adoption pressure; neither loosening nor tightening is safe for repos to choose (#223).
    pub adoption: crate::commands::workflow::adoption::AdoptionPolicy,
    /// Operator-only: whether zirv starts a workflow on a session's first prompt; a repo must not start workflows (and their approvals and spend) for itself.
    pub auto_start: crate::commands::workflow::adoption::AutoStartPolicy,
    /// Operator-only check-child env additions; never replace built-ins or let untrusted checks widen env access (#233).
    pub check_env_passthrough: Vec<String>,
    /// Operator-only reviewer budget limit.
    pub review_worker_budget_tokens: Option<u64>,
    /// Operator-only reviewer tool-call limit.
    pub review_worker_max_tool_calls: Option<u32>,
    /// Opt-in automatic gate workers; repos cannot enable spending on their own behalf (#242).
    pub auto_spawn_on_gate: bool,
    /// Opt-in empty verification pass; otherwise no checks prove nothing, and repos cannot grant themselves a pass (#268).
    pub allow_empty_verify: bool,
    /// Operator-only built-in check exclusions; repos must never disable the checks that police them (#276).
    pub builtin_checks_exclude: Vec<String>,
    /// Operator-only skill-context byte cap for injection and CLI output; always report omitted bytes (#326).
    pub max_context_bytes: usize,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            repo_checks_enabled: true,
            repo_skills_enabled: true,
            repo_agents_enabled: false,
            repo_workflows_enabled: false,
            deploy: WorkflowDeployConfig::default(),
            maintain: WorkflowMaintainConfig::default(),
            telemetry_enabled: true,
            telemetry_max_events: 1000,
            telemetry_retention_days: 30,
            adoption: crate::commands::workflow::adoption::AdoptionPolicy::default(),
            auto_start: crate::commands::workflow::adoption::AutoStartPolicy::default(),
            check_env_passthrough: Vec::new(),
            review_worker_budget_tokens: None,
            review_worker_max_tool_calls: None,
            auto_spawn_on_gate: false,
            allow_empty_verify: false,
            builtin_checks_exclude: Vec::new(),
            max_context_bytes: 8192,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// Master memory switch: false disables every scope regardless of individual scope switches.
    pub enabled: bool,
    /// Automatic handoff harvest is opt-in; durable entries are deliberate by default.
    pub harvest: bool,
    /// Prune oldest entries by Written timestamp beyond this count.
    pub max_entries: usize,
    /// Oversized entry bodies are truncated rather than rejected.
    pub max_entry_bytes: usize,
    /// Deprecated, parsed and repo-forbidden for config compatibility; injection uses `core_max_bytes` (#34).
    pub max_injected_bytes: usize,
    /// Repo-owned shared-memory switch, always subordinate to the master memory switch.
    pub shared_enabled: bool,
    /// Hard received-byte cap for merged core memory with private-first precedence, independent of storage limits.
    pub core_max_bytes: usize,
    /// Independent byte cap for ranked memory added above the core layer (#35).
    pub retrieval_max_bytes: usize,
    /// Independent retrieved-entry count cap prevents many small matches overwhelming the session.
    pub retrieval_max_entries: usize,
    /// Per-session harvest count cap, independent of whole-repo bootstrap limits (#37).
    pub harvest_max_entries: usize,
    /// Cumulative per-session harvest byte cap, independent of single-entry and bootstrap-input caps (#37).
    pub harvest_max_bytes: usize,
    /// Session-memory default destination when identity exists; subordinate to the master switch (#295).
    /// Explicit repo/global scope bypasses this default.
    pub session_enabled: bool,
    /// Journal retention is independent of entry count because forget/verify/promote/rollback also append records (#295).
    pub journal_max_entries: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            harvest: false,
            max_entries: 50,
            max_entry_bytes: 512,
            max_injected_bytes: 2048,
            shared_enabled: true,
            core_max_bytes: 2048,
            retrieval_max_bytes: 2048,
            retrieval_max_entries: 6,
            harvest_max_entries: 5,
            harvest_max_bytes: 2048,
            session_enabled: true,
            journal_max_entries: 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    /// Operator-only aggregate parent-outcome cap; keep newest parents whole and explicitly report dropped bytes (#326).
    pub max_parent_outcome_bytes: usize,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_parent_outcome_bytes: 4096,
        }
    }
}

/// Experimental operator-only PTY persistence: repos cannot extend session lifetimes or persist terminal output (#352).
/// Disabled by default; the table has no effect until persistent mode is enabled.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Opt-in persistent runtime until crash, upgrade and cross-platform recovery are proven.
    pub persistent: bool,
    /// Opt-in disk persistence across runtime restarts, warned at use because terminal output can contain sensitive data.
    /// Detach/reattach needs only the service's live in-memory screen.
    pub history: bool,
    /// In-memory reattach scrollback; independent of disk history.
    pub scrollback_rows: usize,
    /// Heartbeat age is secondary: process-start identity decides staleness, never age or PID alone.
    pub stale_after_secs: u64,
}

impl SessionConfig {
    pub const DEFAULT_SCROLLBACK_ROWS: usize = 2000;
    pub const DEFAULT_STALE_AFTER_SECS: u64 = 120;

    /// Zero uses the built-in scrollback budget rather than retaining nothing.
    pub fn scrollback_rows_or_default(&self) -> usize {
        if self.scrollback_rows == 0 {
            Self::DEFAULT_SCROLLBACK_ROWS
        } else {
            self.scrollback_rows
        }
    }

    pub fn stale_after_secs_or_default(&self) -> u64 {
        if self.stale_after_secs == 0 {
            Self::DEFAULT_STALE_AFTER_SECS
        } else {
            self.stale_after_secs
        }
    }
}

/// Setup bookkeeping gates no repo execution or spend, so it is repo-settable (#87, #93, #95).
/// Setup itself writes only operator-global config, never the repo layer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupConfig {
    /// Prune only on backup writes, never read-only listing; always pin the oldest run outside the cap.
    /// Zero uses the default and values above the ceiling clamp down.
    pub backup_retention_runs: usize,
    /// Record either answer so declining harvest does not prompt again on every setup run.
    pub memory_harvest_offered: bool,
    /// Record that setup offered to wrap a custom Claude statusline.
    pub statusline_wrap_offered: bool,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            backup_retention_runs: 20,
            memory_harvest_offered: false,
            statusline_wrap_offered: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChromeConfig {
    /// Launch disclosure of the resolved harness, selection rule and session id.
    pub banner: bool,
    /// Reserved bottom status bar.
    pub bar: bool,
    /// The `zirv ▸` announcement channel on stderr.
    pub events: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            banner: true,
            bar: true,
            events: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashConfig {
    pub enabled: bool,
    /// Sidebar width is clamped to the frame and hidden below 100 total columns.
    pub sidebar_cols: u16,
    /// Ignore quit-time restore rosters older than this.
    pub roster_max_age_secs: u64,
    /// Pane cap includes the orchestrator and matches selectable slots; enforce it on all non-launch creation to prevent fork bombs.
    pub max_panes: usize,
    /// Mouse capture enables wheel scrolling but requires Shift for native text selection.
    /// Enabled for discoverability; disabling it leaves keyboard scrollback available.
    pub mouse: bool,
    /// Use output quiescence only for adapters without turn signals, otherwise idle-gated mail and nudges never drain.
    /// Repo-settable timing for an already authorized interactive session; this grants no authority.
    pub idle_quiet_ms: u64,
    /// Constrain canonical pane workdirs to allowed roots so forged same-uid requests cannot gain arbitrary repo writes (#228, #179).
    /// Repo root and its parent are always allowed; additional roots are operator-only, with env replacing file values.
    /// Invalid paths remain literal so typos narrow access rather than abort loading.
    pub workdir_roots: Vec<String>,
    /// Repo-settable animation choice changes presentation only, never supervision or authority.
    pub motion: DashMotion,
}

impl Default for DashConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sidebar_cols: 28,
            roster_max_age_secs: 604_800,
            max_panes: 9,
            mouse: true,
            idle_quiet_ms: 10_000,
            workdir_roots: Vec::new(),
            motion: DashMotion::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    /// Repo-settable interactive model is allowed only because launch announces the choice through `chrome.events`.
    /// The repo can hide the banner but cannot suppress this channel; only operator quiet settings may do so.
    pub model: Option<String>,

    /// Operator-only interactive Claude permission mode; repos cannot widen their own posture (#504).
    /// Unset uses default; headless stays dontAsk, and allow/deny lists are unchanged.
    /// Only default, acceptEdits and bypassPermissions are accepted; invalid values hard-error at load.
    pub claude_permission_mode: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_defaults_inject_with_a_capped_repo_layer() {
        let prompt = PromptConfig::default();
        assert!(prompt.enabled);
        assert!(prompt.repo_layer);
        assert_eq!(prompt.max_repo_bytes, 4096);
        assert!(prompt.harnesses);
        assert!(prompt.codex_orchestrator);
    }

    #[test]
    fn context_defaults_match_prompt_max_repo_bytes() {
        let context = ContextConfig::default();
        assert_eq!(context.max_common_bytes, 4096);
        assert_eq!(context.max_harness_bytes, 4096);
        assert_eq!(context.max_harness_roster_bytes, 4096);
    }

    #[test]
    fn mail_defaults_are_enabled_with_sane_caps() {
        let mail = MailConfig::default();
        assert!(mail.enabled, "the mailbox is on by default");
        assert_eq!(mail.max_message_bytes, 4096);
        assert_eq!(mail.max_delivered_bytes, 4096);
        assert_eq!(mail.keep, 50);
    }

    #[test]
    fn model_discovery_and_price_fetch_default_on_without_pins() {
        let models = ModelsConfig::default();
        assert!(models.discovery);
        assert!(models.price_fetch);
        assert!(models.pin.is_empty());
        assert!(models.avoid.is_empty());
        assert!(!models.auto_avoid);
    }

    #[test]
    fn chrome_defaults_are_all_on() {
        let chrome = ChromeConfig::default();
        assert!(chrome.banner, "the launch banner is on by default");
        assert!(chrome.bar, "the status bar is on by default");
        assert!(chrome.events, "the announcement channel is on by default");
    }

    #[test]
    fn memory_defaults_are_enabled_off_harvest_with_sane_caps() {
        let memory = MemoryConfig::default();
        assert!(memory.enabled, "the private memory bank is on by default");
        assert!(
            !memory.harvest,
            "automatic harvesting is off by default: remembering is a deliberate act"
        );
        assert_eq!(memory.max_entries, 50);
        assert_eq!(memory.max_entry_bytes, 512);
        assert_eq!(memory.max_injected_bytes, 2048);
        assert!(
            memory.shared_enabled,
            "the shared (repo-owned) scope is on by default too"
        );
        assert_eq!(memory.core_max_bytes, 2048);
        assert_eq!(memory.retrieval_max_bytes, 2048);
        assert_eq!(memory.retrieval_max_entries, 6);
        assert_eq!(
            memory.harvest_max_entries, 5,
            "one session's own harvest stays conservative by default"
        );
        assert_eq!(memory.harvest_max_bytes, 2048);
    }

    /// Issue #326 B1: default budget for `task::compile_task_prompt`'s own
    /// `## PARENT OUTCOMES` block.
    #[test]
    fn task_config_defaults_to_a_4096_byte_parent_outcome_budget() {
        assert_eq!(TaskConfig::default().max_parent_outcome_bytes, 4096);
    }

    #[test]
    fn dash_defaults_are_on_with_a_28_col_sidebar() {
        let cfg = CtxConfig::default();
        assert!(cfg.dash.enabled);
        assert_eq!(cfg.dash.sidebar_cols, 28);
        assert_eq!(cfg.dash.roster_max_age_secs, 604_800);
        assert_eq!(
            cfg.dash.max_panes, 9,
            "the default cap matches Ctrl+A 1..9 addressing"
        );
        assert!(
            cfg.dash.mouse,
            "the wheel scrolls a pane's scrollback out of the box"
        );
        assert_eq!(cfg.dash.idle_quiet_ms, 10_000);
        assert_eq!(cfg.dash.motion, DashMotion::Full);
    }

    #[test]
    fn chat_model_defaults_to_none() {
        assert_eq!(ChatConfig::default().model, None);
    }
}
