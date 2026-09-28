use super::*;

/// Which model decider `proxy::decide` runs first, per `[proxy] decider`.
/// The chain always falls through toward `Deterministic` on failure (see
/// `proxy::mod::decide`'s own doc comment); this only picks where the chain
/// STARTS -- `Helper` skips `Typesafe` outright, and `Deterministic` skips
/// every model call and returns the baseline classification as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyDecider {
    #[default]
    Typesafe,
    Helper,
    Deterministic,
}

/// The floor `load`'s own range check holds `proxy.request_max_bytes` to --
/// small enough that a request could still be truncated to something
/// legible for a model decider (never literally 0, which would give a
/// decider an empty request every time), far below the `16_384` default.
pub const MIN_PROXY_REQUEST_MAX_BYTES: usize = 1024;

/// Issue #537 seam: the harness proxy's own decision core (`proxy::decide`),
/// disabled by default so every launch path stays byte-identical to today
/// until an operator opts in. `REPO_FORBIDDEN` as a whole -- every key here
/// paired with its own `ZIRV_CTX_PROXY_*` env var (see `REPO_FORBIDDEN`'s own
/// table below): a repository checkout must not be able to turn the proxy on
/// for itself, choose which decider spends the operator's Jev/helper-model
/// budget, or loosen the confidence floor/request cap that bounds it -- the
/// same trust asymmetry `agent`/`handoff.model`/`endpoint` already hold.
///
/// `min_confidence`, `typesafe.timeout_secs` and `request_max_bytes` are
/// range-checked once, in `CtxConfig::load` (see that function's own
/// `proxy.*` block, alongside the `fallback.*`/`chat.model` range and
/// charset checks it already makes), the same "loud rather than silent,
/// load-time error naming the key" convention this crate holds to for every
/// other bounded numeric config value -- never a silent clamp. Reading
/// `cfg.proxy.typesafe.timeout_secs`/`request_max_bytes` anywhere past that
/// point can therefore trust the bound without re-checking or re-flooring.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub decider: ProxyDecider,
    /// A model answer below this per-field confidence is discarded in favor
    /// of the deterministic baseline (`proxy::decision::merge`). Must be in
    /// `0.0..=1.0`.
    pub min_confidence: f32,
    /// A model answer whose margin (`jev::Answer::margin` -- the gap between
    /// its top and runner-up probability) falls below this floor is ALSO
    /// discarded in favor of the deterministic baseline, alongside (never
    /// instead of) `min_confidence` above -- see `jev::Answer::decisive`'s
    /// own doc comment for why confidence alone misses an unstable answer.
    /// Must be in `0.0..=1.0`.
    pub min_margin: f32,
    /// The intake `request` text is truncated to this many bytes before it
    /// ever reaches a model decider (`proxy::decision::IntakeState`). Must be
    /// at least [`MIN_PROXY_REQUEST_MAX_BYTES`].
    pub request_max_bytes: usize,
    pub typesafe: ProxyTypesafeConfig,
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
        }
    }
}

/// TypeSafe's Jev endpoint (`docs.typesafe.ai`), the proxy's primary decider.
/// `credential_env` NAMES the environment variable holding the API key --
/// never the secret itself, the same `EndpointTarget::credential_env`
/// contract this mirrors.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyTypesafeConfig {
    pub base_url: String,
    pub credential_env: String,
    /// Pinned to a specific Jev release by default (`jev-1.13.0`, what
    /// `jev-latest` itself resolves to as of 2026-09-18) rather than the
    /// `jev-latest` moving alias -- reproducibility across FUTURE alias
    /// moves is the point, not that today's alias is wrong: a 2026-09-18
    /// measurement found the model itself flips a thin-margin answer between
    /// otherwise-identical calls, and an alias that can change underneath a
    /// deployed config would confound that instability with an actual model
    /// upgrade. Set this to `jev-latest` explicitly to opt back into
    /// automatic upgrades, or to a newer pinned version once one is
    /// verified.
    pub model: String,
    /// Connect and receive timeout for the `/systemone` call. Must be at
    /// least 1 (see `ProxyConfig`'s own doc comment on where this is
    /// checked).
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

/// Issue #537 seam extraction (task A1): which advisory sites, beyond the
/// harness proxy itself, may consult the shared Jev client (`jev::ask`) in
/// place of their own pre-existing deterministic path. A site is active
/// only when BOTH its key here is `true` AND `jev::available` reports the
/// `[proxy.typesafe]` credential set -- either being false means that
/// site's deterministic path runs byte-identical to today. Connection
/// settings (endpoint, credential, model, timeout) stay in
/// `[proxy.typesafe]`, shared with the harness proxy; this table only ever
/// gates WHICH sites may spend through them. `REPO_FORBIDDEN`, one leaf
/// entry per key, same trust asymmetry as `[proxy]` above: a repository
/// checkout must not be able to turn on a Jev-backed decision path for
/// itself.
///
/// Issue #803: no longer `Eq` -- `floors` (below) carries `f32` fields, which
/// has no total order.
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
    /// Issue #781: a safety-hook Jev risk check that may only ESCALATE a
    /// deterministic `allow` verdict to `ask`/`deny`, never loosen one.
    pub approve: bool,
    /// Issue #781: opt-in auto-approve on top of `approve` -- Jev may LOWER
    /// a deterministic `ask` verdict to `allow`. Effective only when
    /// `approve` is also `true`; `approve_allow` alone (with `approve`
    /// false) never runs a Jev call and never changes a verdict.
    pub approve_allow: bool,
    /// Issue #782: Jev intent refinement for a plain `zirv workflow start`/
    /// `classify` with no other routing signal; `classify` also adds
    /// additive domain tags to its own `ExecutionProfile` output (`start`
    /// classifies before one exists, so it gets intent only).
    pub classify: bool,
    /// Issue #783: Jev keep/drop scoring of handoff candidate items.
    pub handoff_select: bool,
    /// Issue #798: Jev-ranked keep list appended to a compaction's own focus
    /// text.
    pub compaction_select: bool,
    /// Issue #784: Jev prompt-injection screening of untrusted inputs.
    pub inject_screen: bool,
    /// Issue #785: Jev inject-now/defer gate for automatic compact/restart/
    /// mail/Stop injections.
    pub inject: bool,
    /// Issue #786: Jev Stop-hook check for unverified completion claims.
    pub stop_verify: bool,
    /// Off by default: when the deterministic missing-tests Stop gate
    /// (`[missing_tests_gate]`) is about to block, asks Jev one metadata-only
    /// Noul question from local numeric facts (non-test source files
    /// changed, changed-lines bucket, whether the repo has any test files,
    /// how many mention a changed module, doc-only share) -- "is a new test
    /// owed for this change?". A decisive "not owed" answer skips that one
    /// block without persisting it as blocked; anything else (indecisive, an
    /// error, no credential, or this key off) blocks exactly as the
    /// deterministic gate already does.
    pub missing_tests: bool,
    /// Jev refinement of `[headless.effort]`'s own deterministic pick, at a
    /// headless launch's first turn only: a metadata-only low/high call
    /// (`exec.rs`'s `sticky_headless_effort`) that may steer the launch
    /// toward `headless.effort.trivial` (low) or `headless.effort.substantial`
    /// (high) instead of the plain classifier's own class. Effective only
    /// when `[headless.effort]` itself has at least one key set -- with none
    /// set, `apply_headless_cost_levers` never reaches the sticky decision at
    /// all, Jev included. An indecisive, failed, or unavailable answer falls
    /// back to the deterministic class exactly as with the gate off, and the
    /// chosen value is recorded through the same sticky, per-session record
    /// as the deterministic path, so a resumed session never re-asks or
    /// changes effort mid-conversation.
    pub launch_effort: bool,
    /// How long a cached answer (`<state_dir>/jev-cache/<hash>.json`, keyed
    /// by the exact request body -- see `jev::ask`'s own doc comment) stays
    /// usable, in seconds. `0` disables the cache entirely: every call
    /// reaches the network, and none is ever written. Shared by every
    /// `[jev]`-gated site and the harness proxy's own `typesafe` decider,
    /// since both go through the same `jev::ask`. Defaults to a week: Jev
    /// samples a fresh confidence on every uncached call, so for an input
    /// whose confidence straddles a site's floor the cache is the only thing
    /// that keeps the acted decision identical (2026-09-27 determinism
    /// campaigns); the model is part of the key, so a longer TTL never
    /// serves an answer from a different model.
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
            cache_ttl_secs: 604_800,
            floors: JevFloorsConfig::default(),
        }
    }
}

/// One tunable site's confidence/margin floor override -- `None` in either
/// field means "use that call site's own compiled default", so an operator
/// may raise (or lower) just the one number they care about without pinning
/// the other. Both, when set, are validated to `[0.0, 1.0]` at load time
/// (`CtxConfig::load`), the same "loud rather than silent" convention
/// `proxy.min_confidence`/`proxy.min_margin` already hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevSiteFloor {
    pub min_confidence: Option<f32>,
    pub min_margin: Option<f32>,
}

/// Issue #803: `[jev.floors.<site>]` -- tunable per-site overrides for the
/// confidence/margin floors nine advisory sites already check via
/// `jev::Answer::decisive`. Unset (the shipped default) is BYTE-IDENTICAL to
/// today's behaviour: every site keeps calling `decisive` with its own
/// existing compiled constant (see `jev::floor`). Only these nine sites are
/// tunable at all -- a safety/verification site (`approve`, `approve_allow`,
/// `inject_screen`, `stop_verify`, `missing_tests`, and the deterministic
/// `review`/`gates` checks) keeps its compiled floor no matter what a
/// `[jev.floors]` table says, because none of those keys exist here to set.
/// `REPO_FORBIDDEN` as the WHOLE `[jev.floors]` table, one entry (like
/// `[capabilities]`/`[runtime]` above) rather than one per leaf: every key
/// under it loosens or tightens which advisory answers a session acts on,
/// the same trust asymmetry as `[jev]` itself.
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

/// Issue #788: operator-only, off-by-default cost levers for the Claude Code
/// sessions zirv launches HEADLESSLY (`-p`/`--print`) -- `ctx exec`'s
/// `--prompt` path and its `-- claude -p ...` passthrough, plus `zirv agent
/// claude` headless workers (they share `ctx exec`'s own launch builder).
/// Interactive `wrap`/`chat`/dash sessions never read this table. Every key
/// is `REPO_FORBIDDEN`, same trust asymmetry as `[jev]` above: a repo
/// checkout must not be able to turn on prompt-cache billing behavior,
/// per-request effort, or a narrower tool/memory surface for itself. With
/// every key unset (the shipped default) a headless launch is byte-identical
/// to one built before this table existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadlessEffortConfig {
    pub trivial: Option<String>,
    pub bounded: Option<String>,
    /// Also what a request classifying `Complexity::Architectural` reads:
    /// the deterministic classifier this table's own caller uses
    /// (`proxy::decision::try_classify_request`) is TEXT-ONLY (no paths/
    /// changed lines), and `infer_complexity`/the request-size floor it
    /// folds in can never return `Architectural` from text alone -- so
    /// there is no separate `architectural` key to configure.
    pub substantial: Option<String>,
}

/// Issue #788: `[headless]` itself -- see [`HeadlessEffortConfig`]'s own doc
/// comment for the trust/scope statement shared by every key here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadlessConfig {
    /// `"5m"` or `"1h"`, unset by default. When set, a headless claude
    /// launch gets env `CLAUDE_CODE_PROMPT_CACHE_TTL=<value>` -- skipped
    /// when the operator's own process environment already sets
    /// `CLAUDE_CODE_PROMPT_CACHE_TTL`, `FORCE_PROMPT_CACHING_5M` or
    /// `ENABLE_PROMPT_CACHING_1H` (the operator's own env wins).
    pub prompt_cache_ttl: Option<String>,
    /// Per intake-complexity `CLAUDE_CODE_EFFORT_LEVEL`, from the same
    /// deterministic classifier the intake hook uses
    /// (`proxy::decision::try_classify_request`), text-only by default --
    /// `[jev] launch_effort` may steer a first launch's pick toward `trivial`
    /// or `substantial` instead, from local numeric facts only (see that
    /// field's own doc comment); with the gate off this stays a pure
    /// classifier lookup, never a Jev call. Every class unset by default;
    /// skipped when the operator's own process environment already sets
    /// `CLAUDE_CODE_EFFORT_LEVEL` or the claude argv already carries
    /// `--effort`.
    pub effort: HeadlessEffortConfig,
    /// When true, a headless launch's settings layer adds
    /// `"autoMemoryEnabled": false` and `"disableBundledSkills": true`.
    pub lean: bool,
    /// Extra tool names appended to a headless launch's `--disallowedTools`
    /// deny list. Empty by default.
    pub disallowed_tools: Vec<String>,
}

/// Per-agent override for which model runs code review, keyed the same way
/// as `UseCreditsConfig` (operator thinks in agent names). `None` -- the
/// default for both -- defers to that adapter's own `AgentAdapter::
/// review_model_below` ladder, computed one tier below the orchestrator
/// seat's model (`chat.model`, or the top tier when unset). `REPO_FORBIDDEN`
/// as a whole table: a repo checkout must not be able to choose which model
/// spends the operator's vendor account running review, the same asymmetry
/// as `handoff.model`/`optimize.model` above. See `resolve_review_model` in
/// `adapters/mod.rs`, the one place both halves (operator override, ladder
/// default) are combined into the harness-roster line an Orchestrator
/// session actually sees.
///
/// That trust claim is fully true only of this table's own keys directly: a
/// repo checkout can still shift the *derived* ladder default indirectly, by
/// setting `chat.model` (deliberately repo-settable -- see that field's own
/// comment -- and disclosed on screen via `announce_model_choice`, unlike
/// this table). That indirection is accepted because it is disclosed the
/// same way a direct `chat.model` choice is, and it can only ever move the
/// ladder default, never set an explicit `review.<agent>` value outright.
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
