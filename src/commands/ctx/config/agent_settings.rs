use super::*;

/// Per-agent override for which model a delegated headless worker
/// (`zirv ctx agent <name> "<prompt>"`, and the dashboard's own
/// spawn-request pane variant) launches on. Named `worker`, not `agent`,
/// because a top-level `agent` key already exists (default-agent
/// selection) -- this section is not that.
///
/// `None` -- the default for both -- defers to that adapter's own hard
/// default (`AgentAdapter::default_worker_model`: `"sonnet"` for claude,
/// none for codex, whose own CLI/config default applies untouched). See
/// `adapters::worker_model_args`, the one place both halves (operator
/// override, adapter-owned default) are combined into the argv a
/// delegation spawn actually launches with.
///
/// `claude`/`codex` are `REPO_FORBIDDEN` (each its own leaf entry, not the
/// whole table -- see `REPO_FORBIDDEN`'s own comment on this), the same
/// trust asymmetry as `review.claude`/`review.codex` right above: a repo
/// checkout must not be able to choose which model -- and so which vendor
/// account -- spends the operator's tokens running a delegated worker.
/// Unlike `review.*`, which only lands in injected prompt *text*, these keys
/// reach a real launch argv directly (`AgentAdapter::model_args`), so the
/// same charset/length/leading-dash guard `validate_model_str` applies to
/// `chat.model`/`review.*` applies to both keys here too -- see the call
/// sites in `CtxConfig::load`.
///
/// `default_depth`/`default_read_only`/`max_depth`/`deny_network` are issue
/// #262's delegation-envelope defaults (`envelope::WorkerEnvelope`), added
/// to this same table rather than a new one since they are the other half
/// of "what governs a delegated worker".
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
    /// Issue #262: how many hops of further `zirv agent` delegation a ROOT
    /// session's envelope starts with (`envelope::WorkerEnvelope::
    /// delegation_depth`) -- `1` lets a top-level worker delegate exactly
    /// once more before a nested `zirv agent` refuses. `REPO_FORBIDDEN`: a
    /// repo checkout must not be able to grant itself more delegation reach
    /// than the operator configured.
    pub default_depth: u8,
    /// Issue #262: whether a ROOT session's envelope starts read-only
    /// (`destructive: false`, no write paths) when `--mode` was not passed.
    /// `REPO_FORBIDDEN`, same reasoning as `default_depth`.
    pub default_read_only: bool,
    /// Issue #262: an operator ceiling no envelope's `delegation_depth` may
    /// exceed, regardless of `default_depth` or any `--depth` request.
    /// Narrow-only fold with the repo layer (`narrow_worker_max_depth`,
    /// mirroring `narrow_max_nudges`): a repo checkout may only LOWER this,
    /// never raise it above the operator's own value. `u8::MAX` (no extra
    /// cap beyond `default_depth`) is the default.
    pub max_depth: u8,
    /// Issue #262: whether every envelope computed in this repo denies
    /// network tools outright. Narrow-only fold with the repo layer
    /// (`narrow_worker_deny_network`, mirroring `narrow_pace_bool`): a repo
    /// checkout may only turn this ON, never force it back off once the
    /// operator (or another repo layer) has denied network access.
    pub deny_network: bool,
    /// Operator-only whole-run cap for a goal bootstrap before a delegated
    /// worker starts. A repository must not be able to spend the operator's
    /// account by lengthening it.
    pub bootstrap_timeout_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            claude: None,
            codex: None,
            default_depth: 1,
            default_read_only: false,
            max_depth: u8::MAX,
            deny_network: false,
            bootstrap_timeout_secs: 600,
        }
    }
}

/// Issue #718: the warm-worktree pool `zirv ctx agent --worktree
/// --worktree-reuse` draws from -- how many `Idle` trees this repo may keep
/// on disk at once, and how long one may sit unclaimed before `zirv ctx
/// worktree`'s startup GC/`zirv ctx reconcile` retire it through the same
/// proof-required `prune_one` every other removal already goes through.
///
/// Both keys go through the identical T9 repo-narrowing fold `worker.
/// max_depth`/`diagnostics.timeout_secs` already use
/// (`narrow_worktree_idle_pool_max`/`narrow_worktree_idle_ttl_secs` below):
/// a repo checkout may only shrink the pool or shorten the TTL, never grow
/// or lengthen either past the operator's own value.
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

/// Issue #314: the completion-judge policy for an objective-driven `zirv ctx
/// loop` run -- see `judge.rs`'s own module doc for the deterministic-gates-
/// then-cheap-model-verdict shape this configures.
///
/// Every key here goes through the same T9 repo-narrowing fold every other
/// "operator sets the ceiling, a repo checkout may only tighten it" key in
/// this module uses: `gates` may only drop entries from the operator's own
/// list, never add or reorder one (`narrow_objective_gates`, the same shape
/// as `narrow_fallback_order`); `max_cycles_without_progress` may only be
/// lowered, never raised (`narrow_max_cycles_without_progress`, mirroring
/// `narrow_max_nudges`); and `judge` may only be turned OFF, never forced
/// ON (`narrow_objective_judge`, the same polarity as `verify_on_stop.
/// enabled`/`diagnostics.enabled`) -- the judge is a cheap model reading
/// this very repository's own (untrusted) transcript tail, so ON is the
/// looser state: a repo checkout may fall back to gates-and-manual-close
/// only, but may never force the judge on for an operator who left it off.
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

/// Issue #272 (`screen.rs` round 2): thresholds behind `screen::ScreenFlag::
/// RepetitionDominated` (the Hermes-round comment / issue #322). Every key
/// is narrow-only in the SAME direction: a repo checkout may only LOWER a
/// threshold (making detection stricter -- flagging shorter fragments,
/// smaller windows, fewer repeats, or a smaller dominance share), never
/// raise one to make detection looser than the operator's own value
/// (`narrow_screen_threshold`/`narrow_screen_dominance_pct`, both the same
/// `home.min(repo.unwrap_or(MAX))` shape as `narrow_max_nudges`). `screen.rs`
/// itself never reads `ctx.toml`; a caller resolves this table into a
/// `screen::Thresholds` (`ScreenConfig::thresholds`) and passes that to
/// `screen::screen_with_thresholds`, keeping `screen.rs` pure.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenConfig {
    pub repetition_min_fragment: u32,
    pub repetition_window: u32,
    pub repetition_min_repeats: u32,
    pub repetition_dominance_pct: f64,
}

/// Secret and personal-data treatment applied before Zirv-controlled text
/// crosses a model or network boundary. Opt-in: `off` is the default, and
/// only the operator (never a repository checkout) can turn it on (see
/// `REPO_FORBIDDEN`).
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

/// The dash refresh's own motion switch (PR2): `Full` (default) runs
/// clock-driven spinners, the pane-header shimmer, gauge easing, pending-
/// rollover breathing, mail/finished-worker row flashes and toast fades;
/// `Reduced` keeps every STATE change (a spinner still shows working, a
/// gauge still lands on its new value, a toast still appears and expires)
/// and drops only the animation between states. Presentation only -- it
/// never changes what the dashboard supervises or how, so it is not
/// `REPO_FORBIDDEN`, the same reasoning `DashConfig::idle_quiet_ms`'s own
/// doc comment gives for itself.
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
        let text = match std::fs::read_to_string(operator_path()?) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let mut operator: toml::Table = toml::from_str(&text)?;
        let mut merged = toml::Table::new();
        if let Some(value) = operator.remove("obfuscate") {
            merged.insert("obfuscate".into(), value);
        }
        for (var, path, kind) in ENV_MAP {
            if path.first() == Some(&"obfuscate")
                && let Some(raw) = env(var)
            {
                insert_path(&mut merged, path, env_value(&raw, *kind)?);
            }
        }
        match merged.remove("obfuscate") {
            Some(value) => Ok(value.try_into()?),
            None => Ok(Self::default()),
        }
    }

    pub(super) fn fail_closed() -> Self {
        // Issue #466: option loading must fail when the operator policy is unreadable.
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
    /// This table, resolved into the `screen::Thresholds` `screen::
    /// screen_with_thresholds` actually takes -- the one seam between this
    /// (impure, config-reading) module and `screen.rs`'s own purity.
    pub fn thresholds(&self) -> super::super::screen::Thresholds {
        super::super::screen::Thresholds {
            repetition_min_fragment: self.repetition_min_fragment as usize,
            repetition_window: self.repetition_window as usize,
            repetition_min_repeats: self.repetition_min_repeats as usize,
            repetition_dominance_pct: self.repetition_dominance_pct,
        }
    }
}

/// One harness's three generic tiers (`handover::TIERS`), each an optional
/// literal model id overriding that harness's own built-in ladder entry
/// (`handover::tier_default`). `None` -- the default for all three -- defers
/// to the built-in ladder.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoverTierConfig {
    pub cheap: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
}

/// Per-agent model-id overrides for `zirv ctx handover`'s generic tiers
/// (issue #84), keyed by harness the same way `ReviewConfig`/`WorkerConfig`
/// are. Previously env-var-only (`ZIRV_CTX_HANDOVER_<AGENT>_<TIER>`, see the
/// module doc comment on `handover.rs` for why this table did not exist at
/// first); this is the layered `ctx.toml` counterpart, resolved by
/// `handover::resolve_model` with the identical "operator env always wins"
/// precedence every other model choice in this codebase already follows.
///
/// `REPO_FORBIDDEN` as a whole table, the same trust asymmetry as
/// `review.*`/`worker.*` right above and `agent` itself: swapping the
/// orchestrator seat's harness or model is picking which vendor account gets
/// spent, and a repo checkout must not be able to choose that for the
/// operator. `value_at` matches a table node the same way it matches a leaf
/// (see `pace.use_credits`/`review`/`worker` above), so one entry in
/// `REPO_FORBIDDEN` blocks the whole `[handover]` table, both agents, all
/// three tiers.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoverConfig {
    pub claude: HandoverTierConfig,
    pub codex: HandoverTierConfig,
}

/// One adapter's operator-declared model id for each of the three
/// [`workflow::agents::ModelTier`](crate::commands::workflow::agents::
/// ModelTier) routing hints a built-in seat's `AgentManifest.model_tier`
/// already carries (issue #699's cost-routing lever). `None` -- the default
/// for all three -- means the operator has not mapped this tier for this
/// adapter, which `adapters::mod::resolve_tiered_model` reads as "pass no
/// model", never as a built-in ladder to fall back to: unlike
/// `HandoverTierConfig`'s `handover::tier_default`, there is no built-in
/// default here, deliberately, because zirv must never decide on its own
/// that a `Deep` seat may run at a cheaper model. Only the exact tier word
/// the manifest already declares is ever looked up; zirv itself never
/// substitutes a different one.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTierConfig {
    pub fast: Option<String>,
    pub standard: Option<String>,
    pub deep: Option<String>,
}

/// Per-adapter model-tier map for `zirv workflow`'s seat dispatch (issue
/// #699), keyed by harness the same way `ReviewConfig`/`WorkerConfig`/
/// `HandoverConfig` are: `[model_tiers.<adapter>]` with `fast`/`standard`/
/// `deep` keys, each an explicit provider model id. Empty by default (every
/// field of every adapter unset), so an operator who configures nothing
/// changes nothing: `AgentAdapter::dispatch_agent`'s default impl already
/// never guesses a model id when this map has nothing to say, exactly
/// today's behaviour.
///
/// `REPO_FORBIDDEN` as a whole table, the same trust asymmetry as
/// `review.*`/`worker.*`/`handover.*` above and for the identical reason
/// this issue's own design calls out: a repo-owned `<repo>/.zirv/ctx.toml`
/// choosing which model a seat runs on is a provider/model switch, the exact
/// hazard the reverted `resolve_default` change was rejected for (silently
/// picking a different vendor's model than the operator configured is not a
/// narrowing, no matter how the choice is framed). `value_at` matches a
/// table node the same way it matches a leaf (see `pace.use_credits`/
/// `review`/`worker`/`handover` above), so one entry in `REPO_FORBIDDEN`
/// blocks the whole `[model_tiers]` table -- every adapter, every tier --
/// together. Only the operator's own `~/.zirv/ctx.toml`, the matching
/// `ZIRV_CTX_MODEL_TIERS_<AGENT>_<TIER>` env var, or a future explicit flag
/// may set any key here.
///
/// Downgrade note: `deny_unknown_fields` means a zirv release older than
/// this one hard-fails on a persisted key it does not recognise. This
/// change adds exactly one new top-level key (`model_tiers`); an operator
/// config that never sets it deserializes identically before and after this
/// change (`#[serde(default)]` yields the same empty map either way), so a
/// 4.10.0 -> 4.9.0 downgrade stays safe for everyone except an operator who
/// has actually written a `[model_tiers.*]` entry.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTiersConfig {
    pub claude: ModelTierConfig,
    pub codex: ModelTierConfig,
}

/// Issue #395: an operator-only endpoint override pointing one harness at an
/// Anthropic-/OpenAI-compatible vendor endpoint (GLM, Kimi, DeepSeek, Qwen,
/// Mistral, MiniMax, a local Ollama/LM Studio/vLLM runtime) instead of that
/// harness's own native account. `vendor` names a `catalogue::vendor` slug
/// (validated at load, see `CtxConfig::load`'s own endpoint validation block
/// below), so once loaded `catalogue::vendor(&target.vendor)` is infallible
/// in practice -- callers still treat a lookup failure as "fall back to the
/// adapter's native provider" rather than panic, since a stale in-memory
/// config outliving a catalogue change is cheap insurance, not a real
/// expected path.
///
/// `credential_env` is the NAME of an environment variable holding the
/// vendor's API key -- never the secret itself. It is read fresh at launch
/// time (`AgentAdapter::ready()`'s own check, mirrored by `base()`'s env
/// injection for claude and codex's own `env_key` config for the vendor
/// account), and it is never logged, printed, or persisted anywhere.
///
/// `model` is the launch model pinned for this endpoint: the operator's own
/// choice when set, else the vendor's strongest catalogue rung id
/// (`EndpointTarget::pin_model`) -- required at load time for a vendor with
/// no catalogue rungs at all (a local runtime like `ollama`), since there is
/// then no ladder to default from. `wire_api` (codex only) is `"chat"`
/// (default) or `"responses"`.
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
    /// Resolves the actual launch model for this endpoint: the operator's
    /// own `model` when it is set, else the vendor's strongest catalogue rung
    /// id (rungs are stored strongest-first, so the first entry is it). A
    /// `requested` model (an interactive `chat.model`, a handoff ladder tier,
    /// an operator `--model`) is honoured only when it resolves on THIS
    /// endpoint vendor's own ladder, by alias or id -- otherwise it is
    /// silently replaced by the endpoint default, so an Anthropic alias like
    /// `opus` can never reach a GLM endpoint, and a codex ladder tier can
    /// never reach a DeepSeek one. `requested: None` always returns the
    /// endpoint default. Used by both adapters' `model_args` -- the one
    /// place both `claude.rs` and `codex.rs` funnel every `--model`/`-m`
    /// emission through.
    pub fn pin_model(&self, requested: Option<&str>) -> String {
        if let Some(model) = requested
            && let Some(vendor) = super::super::catalogue::vendor(&self.vendor)
            && super::super::catalogue::rung_of(vendor, model).is_some()
        {
            return model.to_string();
        }
        self.default_model()
    }

    /// The endpoint's own default model, with no requested model in hand:
    /// the operator's `model` override, else the vendor's strongest rung id,
    /// else empty (only reachable for a rungless vendor whose `model` load-
    /// time validation already made mandatory, so this is never actually
    /// empty for a loaded config).
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

/// Issue #395: the two harnesses `[endpoint.<agent>]` may retarget. Operator-
/// only (see `REPO_FORBIDDEN`'s whole-table `endpoint` entry): choosing which
/// vendor account a seat spends is the same trust asymmetry `agent`/
/// `review.*`/`worker.*`/`handover.*` above already hold to, applied to the
/// endpoint a harness's own native account is replaced with rather than to
/// which native account is used.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointConfig {
    pub claude: Option<EndpointTarget>,
    pub codex: Option<EndpointTarget>,
}

/// zirv's own shipped-default launch posture (2026-08-22 decision,
/// harness/model parity round): **sandboxed, no prompts**. Commands run
/// freely inside the repository workspace; anything reaching outside it
/// fails rather than prompting a human. `AgentAdapter::default_sandbox_
/// args()` is each adapter's own honest mapping of this posture -- see that
/// method's own doc comment, `adapters::policy_launch_args` (the seam every
/// real launch calls), and the README's "Command safety policy" section
/// (issue #83).
///
/// Independent of `[policy]`/`EffectivePolicy`: that table stays all-`Allow`
/// by default ("zirv's per-capability policy declares nothing"), unchanged
/// by this. This is a separate baseline layered underneath it.
///
/// `REPO_FORBIDDEN`: a repo checkout must not be able to turn its own
/// sandboxing off -- that would be a privilege *widening*, the trust
/// asymmetry every other repo-facing toggle in this table already holds to.
/// The operator's own escape hatch, `[sandbox] enabled = false` in
/// `~/.zirv/ctx.toml` or `ZIRV_CTX_SANDBOX=false`, restores the pre-
/// 2026-08-22 behaviour (no baseline argv from this posture at all; a real
/// launch is then governed purely by `[policy]`, exactly as before this
/// struct existed).
///
/// **`extra_allow`/`extra_deny` (fix round 3, 2026-08-22):** the shipped
/// `adapters::SHIPPED_POSTURE_ALLOW`/`_DENY` lists are deliberately small,
/// so an operator whose project needs one more build command has an
/// escape hatch that does not cost them the whole generated deny list --
/// without one, "pin your own `--allowedTools`" (the only alternative)
/// discards every shipped deny too, which is a worse security posture than
/// a slightly wider allow list. Both are claude permission-rule strings
/// (`ClaudeAdapter::default_sandbox_args`'s own vocabulary; codex has no
/// per-command mechanism to receive them, see that method's doc comment).
///
/// - `extra_allow` is **operator-only**: `["sandbox", "extra_allow"]` is a
///   whole-key `REPO_FORBIDDEN` entry (a repo file setting it at all is a
///   hard load error), so it never needs lifting out of the ordinary deep
///   merge -- a repo can never contribute to it, full stop. Env
///   (`ZIRV_CTX_SANDBOX_EXTRA_ALLOW`, comma-separated) replaces the merged
///   file value outright, the operator's own final word, same as every
///   other `REPO_FORBIDDEN` escape hatch.
/// - `extra_deny` is the one list a repo checkout *may* contribute to --
///   narrowing is always safe. Lifted out of the ordinary deep merge in
///   `CtxConfig::load` (the same treatment `[policy]` gets, for the same
///   reason: a plain merge would let the repo layer's array *replace* the
///   operator's instead of adding to it) and resolved as a **union**: the
///   final list is the operator's home-layer entries plus the repo's,
///   never fewer than either. `ZIRV_CTX_SANDBOX_EXTRA_DENY` (comma-
///   separated) replaces the unioned value outright when set -- the
///   operator's own escape hatch to loosen a repo-added entry, the same
///   "environment wins outright in both directions" rule `[policy]`'s own
///   env layer already holds.
///
/// Both extra lists are appended after the shipped ones in `default_
/// sandbox_args`, so deny continues to beat allow across every source --
/// verified live for the shipped pair, and true here by construction: the
/// underlying CLI mechanism does not care which list an entry came from.
///
/// **`scrub_subprocess_env` (issue #329):** whether the generated claude
/// launch settings set `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1`. Off by default.
/// Read straight from the installed Claude Code binary (2.1.259), that
/// switch does three things, none of which zirv's launch posture wants
/// unasked: it strips a fixed list of the operator's own tool-config and
/// auth-channel variables from EVERY Bash subprocess, sandboxed or not --
/// `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `GIT_SSH_COMMAND`, `GH_CONFIG_DIR`,
/// `DOCKER_CONFIG`, `KUBECONFIG`, `GNUPGHOME` and the like -- which is
/// exactly why the `env.SSH_AUTH_SOCK` the same settings file exports never
/// reached a single `git fetch` (#329 item 1); it forces the permission mode
/// to `default`, silently overriding the `dontAsk` a headless launch pins;
/// and it is documented upstream as `allowed_non_write_users` hardening for
/// CI runners, not interactive operator sessions. Credential FILES stay
/// unreadable inside the sandbox regardless (`sandbox.filesystem.denyRead`
/// plus the `Read(...)` deny rules), so turning the scrub off costs only the
/// stripping of credential-bearing environment variables from subprocesses.
///
/// `REPO_FORBIDDEN`, whole key, like `enabled`: the switch changes the
/// launch's permission mode as a side effect, which is the operator's
/// posture to set, not a repo checkout's. `ZIRV_CTX_SANDBOX_SCRUB_SUBPROCESS_
/// ENV` is the operator's own final word.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub extra_allow: Vec<String>,
    pub extra_deny: Vec<String>,
    pub scrub_subprocess_env: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_allow: Vec::new(),
            extra_deny: Vec::new(),
            scrub_subprocess_env: false,
        }
    }
}

/// Cross-harness routing policy (issue #186). The operator controls the
/// broad policy in `~/.zirv/ctx.toml`; the repository layer is folded
/// asymmetrically in `CtxConfig::load` so it may only make this feature less
/// eager: disable it, remove candidates, require more candidate headroom,
/// assume less capacity for an unknown signal, or lower the definition of a
/// bounded "small" task. Environment variables are the operator's final word.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackConfig {
    /// Master switch for automatic cross-harness routing and blocked-session
    /// continuation.
    pub enabled: bool,
    /// Stable preference order. Runtime selection still prefers the candidate
    /// with more known/assumed headroom; this order breaks ties.
    pub order: Vec<String>,
    /// New background work may be steered away from its requested harness once
    /// that harness has less than this much percentage headroom.
    pub predictive_headroom_pct: f64,
    /// A fallback candidate must have at least this much percentage headroom.
    pub min_candidate_headroom_pct: f64,
    /// Conservative synthetic headroom for an enabled/ready harness whose usage
    /// signal is absent or stale. Set to 0 to opt such harnesses out.
    pub unknown_headroom_pct: f64,
    /// A capacity-limited ("small tasks only") harness may only receive work
    /// with an explicit token ceiling at or below this value.
    pub small_task_max_tokens: u64,
    /// Or an explicit tool-call ceiling at or below this value. At least one
    /// bounded dimension is required before a small-capacity harness qualifies.
    pub small_task_max_tool_calls: u32,
    /// Issue #358: whether background delegation may shift work across
    /// harnesses adaptively as measured headroom changes, rather than only at
    /// dispatch time. Repo narrowing is AND, the same as `enabled`: a repo
    /// checkout may disable it, never enable it for an operator who did not.
    pub adaptive_delegation: bool,
    /// Whether the orchestrator seat itself may roll over onto the next
    /// fallback candidate automatically, not just newly-dispatched
    /// background work.
    ///
    /// `None` -- the default -- means "decide from the roster", and
    /// [`CtxConfig::auto_orchestrator_rollover`] is the one place that
    /// decision is made: ON whenever more than one harness in
    /// `fallback.order` is enabled, OFF when there is nowhere to roll over
    /// to. That reverses issue #358's own "off by default" (Decision Log
    /// (d)): with one harness the switch is meaningless, and with two an
    /// operator who set up cross-harness fallback at all wants the seat to
    /// follow the capacity, not to sit on an exhausted account until they
    /// notice. An explicit `false` in `~/.zirv/ctx.toml` (or
    /// `ZIRV_CTX_FALLBACK_AUTO_ORCHESTRATOR_ROLLOVER`) still wins outright.
    ///
    /// Repo narrowing is AND, same as `enabled`/`adaptive_delegation`, and
    /// keeps working against an unset home layer: a repo may set `false`
    /// (narrowing, so it sticks), and a repo `true` is discarded (widening),
    /// leaving the roster default in force.
    pub auto_orchestrator_rollover: Option<bool>,
    /// Issue #358: the headroom threshold that triggers `auto_orchestrator_
    /// rollover`. `None` (the default) means "inherit `predictive_headroom_
    /// pct`" -- see `rollover_headroom_pct`. `REPO_FORBIDDEN`: rolling the
    /// operator's own seat is the same class of decision `handoff.model`/
    /// `optimize.model` already gate -- a repo checkout must not be able to
    /// tune when that happens.
    pub orchestrator_rollover_headroom_pct: Option<f64>,
    /// Issue #358: the minimum gap between two automatic orchestrator
    /// rollovers, so a harness bouncing near the threshold cannot thrash the
    /// operator's seat back and forth. `REPO_FORBIDDEN`, same reasoning as
    /// `orchestrator_rollover_headroom_pct`.
    pub rollover_cooldown_secs: u64,
    /// Maximum wait for an idle boundary after a confirmed block.
    /// `REPO_FORBIDDEN`: only the operator may permit a forced seat swap.
    pub reactive_force_after_secs: u64,
    /// Issue #358: per-harness overrides of the global concurrency/headroom
    /// limits above, keyed by adapter name (`fallback.harness.<name>`). A
    /// repo checkout may only lower `max_active` and only raise
    /// `reserve_headroom_pct` per harness name -- see `narrow_fallback_
    /// harness` and `HarnessLimits`' own doc comment.
    pub harness: std::collections::BTreeMap<String, HarnessLimits>,
    /// Issue #455: health-aware routing (`[fallback.health]`). Usage
    /// headroom cannot express "this endpoint refuses connections", so a
    /// per-route circuit breaker is folded in alongside it. `enabled`
    /// narrows like `fallback.enabled` (a repo may switch it off, never on);
    /// the three timing knobs are `REPO_FORBIDDEN`, matching
    /// `rollover_cooldown_secs`/`reactive_force_after_secs` right above --
    /// tuning when zirv stops trusting a vendor route is the same class of
    /// spend decision.
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
    /// The effective headroom threshold for `auto_orchestrator_rollover`:
    /// the explicit override when set, otherwise the same threshold new
    /// background delegation already steers on.
    ///
    /// Issue #358: `seat.rs`'s rollover eligibility check and `rollover.rs`
    /// now read this.
    pub fn rollover_headroom_pct(&self) -> f64 {
        self.orchestrator_rollover_headroom_pct
            .unwrap_or(self.predictive_headroom_pct)
    }

    /// The per-harness limit overrides for `name`, matched case-
    /// insensitively (adapter names are lowercase by convention, but a
    /// hand-edited `ctx.toml` should not silently miss its own override over
    /// a case mismatch). `HarnessLimits::default()` -- both fields `None`,
    /// meaning "use the global limits" -- when `name` has no entry.
    ///
    /// Same issue #358 task-ordering note as `rollover_headroom_pct` above.
    pub fn harness_limits(&self, name: &str) -> HarnessLimits {
        self.harness
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, limits)| limits.clone())
            .unwrap_or_default()
    }

    /// The effective reserve-headroom floor for `name`: its own per-harness
    /// override when set, otherwise the global `min_candidate_headroom_pct`
    /// every candidate is already held to.
    ///
    /// Same issue #358 task-ordering note as `rollover_headroom_pct` above.
    pub fn reserve_headroom_pct(&self, name: &str) -> f64 {
        self.harness_limits(name)
            .reserve_headroom_pct
            .unwrap_or(self.min_candidate_headroom_pct)
    }

    /// Issue #455: the route-health policy every consumer should read, with
    /// the master fallback switch already folded in. Turning cross-harness
    /// fallback off leaves nowhere for a denied route's work to go, so the
    /// breaker must go quiet with it rather than deny work zirv can no
    /// longer reroute.
    pub fn effective_health(&self) -> super::super::health::HealthPolicy {
        super::super::health::HealthPolicy {
            enabled: self.enabled && self.health.enabled,
            ..self.health.clone()
        }
    }
}

/// Per-harness overrides under `[fallback.harness.<name>]` (issue #358).
/// Both fields default to `None`, meaning "use the global `fallback.*`
/// limits unchanged" -- an entry only needs to name the field it actually
/// wants to override. Repo narrowing (`narrow_fallback_harness`) is per
/// field, not whole-entry: `max_active` may only be lowered and `reserve_
/// headroom_pct` may only be raised. A repo-only `max_active` simply
/// applies, since `None` on the home side means "no cap" and any repo value
/// narrows that -- but a repo-only `reserve_headroom_pct` is clamped to the
/// global `min_candidate_headroom_pct` floor, because there `None` means
/// "use that floor", so a lower repo value would be a widening (A-2/D-3).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessLimits {
    /// Ceiling on concurrently active delegations to this harness. `None`
    /// means "no per-harness cap beyond whatever else limits concurrency".
    pub max_active: Option<u32>,
    /// Per-harness reserve-headroom floor, overriding `min_candidate_
    /// headroom_pct` for this one harness. `None` means "use the global
    /// floor" -- see `FallbackConfig::reserve_headroom_pct`.
    pub reserve_headroom_pct: Option<f64>,
}

/// Issue #491 (roadmap N22): which backend a session gets when the caller did
/// not name one. `zirv ctx exec`/`zirv ctx agent` default their `--runtime`
/// flag to the literal `configured`, and `zirv chat` with no `--runtime` at
/// all means the same thing: consult this table, fall back to the harness.
///
/// Opt-in by construction. An absent `[runtime]` table, an absent `default`
/// and an unparsable value all resolve to `harness` -- the behaviour every
/// build before N22 had -- so an existing operator config keeps running the
/// legacy backend until they say otherwise, and saying otherwise is one key.
/// Switching back is deleting that key (or `zirv ctx config migrate
/// --downgrade`, which restores the pre-migration document wholesale).
///
/// The WHOLE `[runtime]` table is `REPO_FORBIDDEN`, the same reasoning
/// `[capabilities]` carries: a checked-out repository moving this operator's
/// sessions onto their metered native provider accounts is pure widening, and
/// there is no narrowing half to allow -- "run on the harness instead" is not
/// a safety property a repo gets to assert either, because the operator's
/// harness account is just as spendable.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// `"native"` or `"harness"`. Unset (the default) means `harness`.
    /// Deliberately a plain `String` rather than a typed enum: an unknown
    /// value here must degrade to the harness with a doctor finding, never
    /// abort the whole config load of a machine whose zirv is older than the
    /// value someone wrote.
    pub default: Option<String>,
    /// Per-role overrides, keyed by the same role names `[roles]` in
    /// `native.toml` uses (`orchestrator`, `worker`, `reviewer`, ...). A role
    /// named here outranks `default`.
    pub roles: std::collections::BTreeMap<String, String>,
}

/// Issue #483 (roadmap N14): the non-shell capabilities a native session has
/// no host harness to inherit -- MCP servers, web search/fetch, browser
/// automation.
///
/// The WHOLE `[capabilities]` table is `REPO_FORBIDDEN`. Every key in it
/// names something zirv then runs, reaches over the network, or authenticates
/// with: an MCP server command, a remote endpoint, a credential reference, a
/// browser binary. A checked-out repository adding any of them is pure
/// widening -- exactly what the repo layer may never do -- so only
/// `~/.zirv/ctx.toml` or `ZIRV_CTX_CAPABILITIES` may set them.
///
/// Everything here is off by default. An unconfigured capability is reported
/// as `unavailable` with a diagnosis naming what is missing; it never
/// degrades into an empty success.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitiesConfig {
    /// Master switch. With this off, no MCP server is contacted, no outbound
    /// web call is made and no browser is launched, whatever else is set.
    pub enabled: bool,
    pub web: WebCapabilityConfig,
    pub browser: BrowserCapabilityConfig,
    /// Configured MCP servers, in declaration order.
    pub mcp: Vec<McpServerConfig>,
    /// At or below this many discovered MCP tools, each one is registered as
    /// its own native tool definition. Above it, the catalogue is reachable
    /// only through the compact index plus an on-demand describe, so a large
    /// toolset never forces every schema into every model request.
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

    /// Servers an operator actually turned on. A disabled entry stays in the
    /// file and out of every session.
    pub fn active_servers(&self) -> impl Iterator<Item = &McpServerConfig> {
        let enabled = self.enabled;
        self.mcp
            .iter()
            .filter(move |server| enabled && server.enabled && !server.name.trim().is_empty())
    }
}

/// Configured web search and fetch. Both are *configured* capabilities: a raw
/// model provides neither, and zirv never claims otherwise.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebCapabilityConfig {
    /// A search endpoint taking `{query}` in its URL template and answering
    /// JSON. Unset means web search is unavailable, not silently empty.
    pub search_endpoint: Option<String>,
    /// `env:NAME`, `store:<item>` or `file:<path>`, resolved through the same
    /// credential store the direct providers use. Never logged.
    pub search_credential: Option<String>,
    /// Whether `web_fetch` may retrieve a URL at all.
    pub fetch_enabled: bool,
    /// Hosts the web capabilities may reach. Empty means none: an allowlist
    /// with no entries is a closed door, not an open one.
    pub allow_hosts: Vec<String>,
    /// Ceiling on one fetched body before it is stored as bounded evidence.
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

/// Configured browser automation. The backend is the same headless
/// Chromium-family binary `frontend render` already drives, so a machine that
/// can capture a frontend render can inspect a page natively too.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserCapabilityConfig {
    pub enabled: bool,
    /// An explicit binary, overriding discovery. Absent means "discover a
    /// Chromium-family browser on PATH, and report unavailable if there is
    /// none".
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

/// One configured MCP server. `effects` is the *trusted* declaration of what
/// this server's tools may do: it comes from the operator's own config and is
/// what the N04 broker admits against. Server-supplied descriptions never
/// influence it.
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

/// How to reach one MCP server. `mode` is explicit: a server is local or
/// remote because the operator said so, never because a URL happened to parse.
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
    /// A remote Streamable HTTP endpoint, optionally bearer-authenticated
    /// from the shared credential store.
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

/// The operator's declaration of what a configured integration's tools may
/// do. Mirrors `runtime::enforcement::ProcessEffects` one field at a time so
/// the config surface and the broker's own vocabulary cannot drift; every
/// field defaults to `false`, so an undeclared effect is unavailable rather
/// than assumed.
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
