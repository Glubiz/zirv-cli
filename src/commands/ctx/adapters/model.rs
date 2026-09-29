//! Provider/account and tiered-model resolution for review and delegated-worker
//! launches.
use super::*;

/// Resolve usage account from the static adapter registry even when that adapter is absent or disabled. (#690)
pub fn provider_for_agent_name(name: Option<&str>) -> &'static str {
    provider_for_agent_and_model(name, None)
}

/// The model-aware sibling of [`provider_for_agent_name`] above: resolves
/// through [`AgentAdapter::provider_for_model`] rather than the adapter's
/// own static `provider()`, for a caller that already has a pinned/
/// resolved model in hand at the point it needs a provider slug for usage/
/// pacing/reservation state -- see that method's own doc comment for why a
/// multi-provider adapter needs this instead of one static slug per
/// registered name. `model: None` reproduces `provider_for_agent_name`
/// exactly, so every existing caller of that function is unaffected by this
/// one's addition. Same registry-only, no-readiness-required lookup, and
/// the same `LEGACY_USAGE_PROVIDER` fallback for an unknown or absent name.
pub fn provider_for_agent_and_model(name: Option<&str>, model: Option<&str>) -> &'static str {
    name.and_then(|n| ADAPTERS.iter().find(|(adapter_name, _)| *adapter_name == n))
        .map(|(_, ctor)| ctor(None).provider_for_model(model))
        .unwrap_or(super::super::window::LEGACY_USAGE_PROVIDER)
}

/// Final wave item 4: `provider_for_agent_name(cfg.agent)` alone gets an
/// *unset* `agent` wrong whenever `resolve_default` would not have landed on
/// the legacy provider -- an operator-disabled claude (home `.settings.toml`
/// or `ZIRV_AGENT_CLAUDE_ENABLED=false`, not a repo one) with codex enabled
/// and ready falls back straight to `LEGACY_USAGE_PROVIDER` ("anthropic")
/// with no `agent` name to derive anything more specific from, even though
/// `resolve_default`'s own fallback loop would correctly skip claude and
/// land on codex. Tried first here for exactly that reason: `resolve_
/// default` is the actual selection logic (gates, `ready()`, the repo-
/// narrowing guard), so when it succeeds its answer is authoritative.
/// `provider_for_agent_name` is the fallback for when it does not -- an
/// explicitly configured, repo-disabled agent (`resolve_default`'s
/// configured arm hard-refuses there) still needs a provider, and only
/// `provider_for_agent_name` can name one without requiring readiness.
///
/// Resolves through `provider_for_model(cfg.chat.model)`, not the static
/// `provider()`: this readout describes the interactive orchestrator seat
/// (`zirv chat`/bare `wrap`), whose own model is `cfg.chat.model` when the
/// operator configured one -- the same field `seat_model_env` and
/// `wrap::run_with`'s `seat_cfg_model` already read for that seat.
pub fn provider_for_usage_readout(cfg: &CtxConfig) -> &'static str {
    // Account identity does not depend on binary presence; a fallback could misattribute usage. (#690)
    resolve_default_with_presence(cfg, &presence_not_consulted)
        .map(|(adapter, _origin)| adapter.provider_for_model(cfg.chat.model.as_deref()))
        .unwrap_or_else(|_| {
            provider_for_agent_and_model(cfg.agent.as_deref(), cfg.chat.model.as_deref())
        })
}

/// Resolve an operator review-model override or the adapter's below-seat default once for roster and launch.
pub(crate) struct ReviewModelChoice {
    pub(crate) model: String,
    pub(crate) configured: bool,
}

/// The reviewer launch uses this resolved model, matching the roster guidance.
pub(crate) fn resolve_review_model(
    cfg: &CtxConfig,
    name: &str,
    adapter: &dyn AgentAdapter,
) -> ReviewModelChoice {
    let configured = match name {
        "claude" => cfg.review.claude.as_deref(),
        "codex" => cfg.review.codex.as_deref(),
        _ => None,
    };
    if let Some(model) = configured {
        return ReviewModelChoice {
            model: model.to_string(),
            configured: true,
        };
    }
    ReviewModelChoice {
        model: adapter
            .review_model_below(cfg.chat.model.as_deref())
            .to_string(),
        configured: false,
    }
}

/// The operator's configured model id for `(adapter, tier)`, from `ctx.
/// toml`'s `[model_tiers.<adapter>]` table (`config::ModelTiersConfig`) --
/// issue #699's cost-routing lever. `None` when the operator has not mapped
/// this exact pair, and `dispatch_agent` must read that as "pass no model",
/// never as license to guess one.
///
/// Unlike [`resolve_worker_model`] below (which falls back to `adapter`'s
/// own hard default) or `handover::resolve_model` (which falls back to a
/// built-in per-vendor ladder), this function has NO fallback of its own:
/// `manifest.model_tier` is a routing hint the manifest already declared,
/// and zirv must never decide by itself that a `Deep` seat may run cheaper
/// than the operator configured. Only the tier word the manifest already
/// carries is ever looked up; this never substitutes a different one.
pub(crate) fn resolve_tiered_model<'a>(
    cfg: &'a CtxConfig,
    adapter: &str,
    tier: crate::commands::workflow::agents::ModelTier,
) -> Option<&'a str> {
    use crate::commands::workflow::agents::ModelTier;
    let tiers = match adapter {
        "claude" => &cfg.model_tiers.claude,
        "codex" => &cfg.model_tiers.codex,
        _ => return None,
    };
    let configured = match tier {
        ModelTier::Fast => tiers.fast.as_deref(),
        ModelTier::Standard => tiers.standard.as_deref(),
        ModelTier::Deep => tiers.deep.as_deref(),
    };
    configured.filter(|model| !model.trim().is_empty())
}

/// Resolve the worker override or adapter default; `None` leaves model selection to the launched agent.
fn resolve_worker_model<'a>(
    cfg: &'a CtxConfig,
    name: &str,
    adapter: &'a dyn AgentAdapter,
) -> Option<&'a str> {
    let configured = match name {
        "claude" => cfg.worker.claude.as_deref(),
        "codex" => cfg.worker.codex.as_deref(),
        _ => None,
    };
    configured.or_else(|| adapter.default_worker_model())
}

/// Argv tokens (`AgentAdapter::model_args`) for the resolved worker model, or
/// empty when `resolve_worker_model` resolves nothing. The one place a
/// delegation spawn (`zirv ctx agent`'s own headless path in `agent.rs`, and
/// the dashboard's own spawn-request pane variant in `dash/mod.rs`) turns the
/// resolved model into a flag; neither caller applies this when the
/// operator's own trailing flags already pin a model explicitly (see each
/// caller's own doc comment for why that check lives there and not here).
pub fn worker_model_args(cfg: &CtxConfig, name: &str, adapter: &dyn AgentAdapter) -> Vec<String> {
    match resolve_worker_model(cfg, name, adapter) {
        Some(model) => adapter.model_args(model),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic seat manifest for the `resolve_tiered_model`/`dispatch_
    /// agent` tests below, parameterised only by the routing hint they need
    /// to probe. Mirrors `dispatch_agent_invariants_hold_for_claude_and_
    /// codex`'s own inline `AgentManifest` construction, with `read_only:
    /// false` and no required capabilities so it dispatches under the
    /// default permissive policy with no extra setup.
    fn tiered_probe_manifest(
        tier: crate::commands::workflow::agents::ModelTier,
    ) -> crate::commands::workflow::agents::AgentManifest {
        use crate::commands::workflow::agents::{AGENT_SCHEMA_VERSION, AgentManifest};
        AgentManifest {
            schema_version: AGENT_SCHEMA_VERSION,
            id: "tiered-probe".to_string(),
            version: 1,
            name: "Tiered Probe".to_string(),
            description: "issue #699 cost-routing lever probe".to_string(),
            role: "worker".to_string(),
            model_tier: tier,
            read_only: false,
            required_capabilities: Vec::new(),
            optional_capabilities: Vec::new(),
            context_budget_bytes: 4096,
            instructions: "Do the thing.".to_string(),
            team_role: None,
            skills: Vec::new(),
        }
    }

    /// Whether `argv` carries a `--model <value>` pair anywhere, the same
    /// flag both `ClaudeAdapter`/`CodexAdapter` `model_args` emit.
    fn argv_model(argv: &[String]) -> Option<&str> {
        argv.windows(2)
            .find(|w| w[0] == "--model")
            .map(|w| w[1].as_str())
    }

    /// Issue #699, the most important property of the whole lever: an
    /// operator who configures nothing (`[model_tiers]` absent from both
    /// layers) sees zero behaviour change -- `dispatch_agent` must still
    /// pass no `--model` flag at all, exactly as it did before this lever
    /// existed. Covers every built-in tier, on both registered adapters, so
    /// no single tier or adapter can quietly start guessing.
    #[test]
    fn dispatch_agent_passes_no_model_when_the_tier_map_is_empty() {
        use crate::commands::workflow::agents::{AgentTask, ModelTier};

        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let task = AgentTask {
            prompt: "do the thing".to_string(),
            repo: repo.path().to_path_buf(),
            model: None,
        };

        for name in ["claude", "codex"] {
            let adapter = select(Some(name), &[], &super::super::tests::permissive_cfg())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            for tier in [ModelTier::Fast, ModelTier::Standard, ModelTier::Deep] {
                let manifest = tiered_probe_manifest(tier);
                let argv = flatten_command(
                    adapter
                        .dispatch_agent(&manifest, &task)
                        .unwrap_or_else(|e| panic!("{name}/{tier}: {e}")),
                );
                assert_eq!(
                    argv_model(&argv),
                    None,
                    "{name}/{tier}: an empty model_tiers map must add no --model flag, got {argv:?}"
                );
            }
        }
    }

    /// A `[model_tiers.<agent>]` entry deserializes and, with the operator's
    /// value set on the HOME layer (never REPO_FORBIDDEN there -- see
    /// `reject_untrusted_keys`, only ever applied to the repo layer), an
    /// unmapped tier for that SAME mapped adapter still falls through to no
    /// model at all: the map is per-(adapter, tier), not a whole-adapter
    /// switch.
    #[test]
    fn dispatch_agent_falls_through_to_no_model_for_an_unmapped_tier_on_a_mapped_adapter() {
        use crate::commands::workflow::agents::{AgentTask, ModelTier};

        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[model_tiers.claude]\nfast = \"haiku-cheap\"\n",
        )
        .expect("write home ctx.toml");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let task = AgentTask {
            prompt: "do the thing".to_string(),
            repo: repo.path().to_path_buf(),
            model: None,
        };

        let adapter =
            select(Some("claude"), &[], &super::super::tests::permissive_cfg()).expect("claude");
        let manifest = tiered_probe_manifest(ModelTier::Standard);
        let argv = flatten_command(adapter.dispatch_agent(&manifest, &task).expect("dispatch"));
        assert_eq!(
            argv_model(&argv),
            None,
            "claude/standard: fast is mapped but standard is not, so this must still add no \
             --model flag, got {argv:?}"
        );
    }

    /// The lever's actual payoff: a mapped `(adapter, tier)` resolves to the
    /// operator's configured model id, reaching the real launch argv.
    #[test]
    fn dispatch_agent_resolves_a_mapped_adapter_and_tier_to_the_configured_model() {
        use crate::commands::workflow::agents::{AgentTask, ModelTier};

        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[model_tiers.claude]\ndeep = \"opus-max\"\n[model_tiers.codex]\ndeep = \"gpt-mega\"\n",
        )
        .expect("write home ctx.toml");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let task = AgentTask {
            prompt: "do the thing".to_string(),
            repo: repo.path().to_path_buf(),
            model: None,
        };
        let manifest = tiered_probe_manifest(ModelTier::Deep);

        for (name, expected) in [("claude", "opus-max"), ("codex", "gpt-mega")] {
            let adapter = select(Some(name), &[], &super::super::tests::permissive_cfg())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let argv = flatten_command(
                adapter
                    .dispatch_agent(&manifest, &task)
                    .unwrap_or_else(|e| panic!("{name}: {e}")),
            );
            assert_eq!(
                argv_model(&argv),
                Some(expected),
                "{name}/deep: expected the configured model in argv, got {argv:?}"
            );
        }
    }

    /// Resolution order rule 1 (issue #699): an explicit `AgentTask::model`
    /// pin always wins over the operator's tier map, even when the map has
    /// an entry for the exact same `(adapter, tier)` pair.
    #[test]
    fn dispatch_agent_prefers_an_explicit_task_model_pin_over_the_tier_map() {
        use crate::commands::workflow::agents::{AgentTask, ModelTier};

        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[model_tiers.claude]\nstandard = \"mapped-model\"\n\
             [model_tiers.codex]\nstandard = \"mapped-model\"\n",
        )
        .expect("write home ctx.toml");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let manifest = tiered_probe_manifest(ModelTier::Standard);

        for name in ["claude", "codex"] {
            let task = AgentTask {
                prompt: "do the thing".to_string(),
                repo: repo.path().to_path_buf(),
                model: Some("pinned-model".to_string()),
            };
            let adapter = select(Some(name), &[], &super::super::tests::permissive_cfg())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let argv = flatten_command(
                adapter
                    .dispatch_agent(&manifest, &task)
                    .unwrap_or_else(|e| panic!("{name}: {e}")),
            );
            assert_eq!(
                argv_model(&argv),
                Some("pinned-model"),
                "{name}: an explicit AgentTask::model pin must win over a mapped tier, got {argv:?}"
            );
        }
    }

    /// Track C (#383): a stand-in for an upcoming multi-provider adapter
    /// (OpenCode/Pi/Goose/Droid-shaped) whose pinned `provider/model` argv
    /// can name a DIFFERENT vendor per launch. `provider()` answers this
    /// adapter's own fallback default; `provider_for_model` reads the vendor
    /// prefix out of a `"<vendor>/<model>"` string when one is given, the
    /// same shape those front ends use.
    #[derive(Debug)]
    struct MultiProviderStubAdapter;

    impl AgentAdapter for MultiProviderStubAdapter {
        fn name(&self) -> &'static str {
            "multi-provider-stub"
        }

        fn program(&self) -> &str {
            "multi-provider-stub"
        }

        fn provider(&self) -> &'static str {
            "anthropic"
        }

        fn provider_for_model(&self, model: Option<&str>) -> &'static str {
            match model.and_then(|m| m.split('/').next()) {
                Some("google") => "google",
                Some("openai") => "openai",
                _ => self.provider(),
            }
        }

        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }

        fn detect(&self, _command: &[String]) -> bool {
            false
        }

        fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
            Command::new("true")
        }

        fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
            Command::new("true")
        }

        fn distiller_cmd(&self, _model: &str) -> Command {
            Command::new("true")
        }

        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
            PathBuf::new()
        }

        fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
            Vec::new()
        }

        fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
            StructuralContext::default()
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
            TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// The default `provider_for_model` (claude/codex; every adapter this
    /// track ships) ignores `model` entirely and always answers the static
    /// `provider()` -- the whole point of a default that is a no-op until an
    /// adapter opts in.
    #[test]
    fn the_default_provider_for_model_ignores_the_model_and_returns_provider() {
        let claude = claude::ClaudeAdapter::new(None);
        assert_eq!(claude.provider_for_model(None), claude.provider());
        assert_eq!(claude.provider_for_model(Some("opus")), claude.provider());
        let codex = codex::CodexAdapter::new(None);
        assert_eq!(
            codex.provider_for_model(Some("gpt-5.6-terra")),
            codex.provider()
        );
    }

    /// A multi-provider adapter's override changes which account a launch
    /// bills depending on the model it pins -- the mechanism the whole
    /// `provider_for_model` addition exists for.
    #[test]
    fn a_multi_provider_adapter_resolves_the_account_from_the_pinned_model() {
        let adapter = MultiProviderStubAdapter;
        assert_eq!(adapter.provider(), "anthropic");
        assert_eq!(
            adapter.provider_for_model(Some("google/gemini-3-pro")),
            "google"
        );
        assert_eq!(adapter.provider_for_model(Some("openai/gpt-5")), "openai");
        // No pinned model, and a model naming no known vendor prefix, both
        // fall back to this adapter's own static default.
        assert_eq!(adapter.provider_for_model(None), "anthropic");
        assert_eq!(adapter.provider_for_model(Some("unqualified")), "anthropic");
    }

    /// This is the exact seam `StateDir::usage_for`/`poll_marker_for` file
    /// names off of (see their own doc comments): a launch pinning a
    /// `"google/..."` model must file its usage/pacing state under
    /// `usage-google.json`, separate from this adapter's own default
    /// `usage-anthropic.json` -- not silently sharing the default account's
    /// window the way the static `provider()` alone would.
    #[test]
    fn usage_for_names_a_different_file_per_pinned_model_provider() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = super::super::super::state::StateDir::from_root(tmp.path().to_path_buf());
        let adapter = MultiProviderStubAdapter;
        assert_eq!(
            state.usage_for(adapter.provider_for_model(Some("google/gemini-3-pro"))),
            tmp.path().join("usage-google.json")
        );
        assert_eq!(
            state.usage_for(adapter.provider()),
            tmp.path().join("usage-anthropic.json"),
            "the static provider() still names the adapter's own default file"
        );
    }

    #[test]
    fn worker_model_args_uses_the_configured_value_over_the_adapter_default() {
        let adapter = claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: Some("opus".to_string()),
                codex: None,
                ..Default::default()
            },
            ..super::super::tests::permissive_cfg()
        };
        assert_eq!(
            worker_model_args(&cfg, "claude", &adapter),
            vec!["--model".to_string(), "opus".to_string()],
            "the operator's own worker.claude wins over the hard default"
        );
    }

    #[test]
    fn worker_model_args_falls_back_to_claudes_hard_sonnet_default() {
        let adapter = claude::ClaudeAdapter::new(None);
        let cfg = super::super::tests::permissive_cfg();
        assert_eq!(cfg.worker.claude, None, "nothing configured");
        assert_eq!(
            worker_model_args(&cfg, "claude", &adapter),
            vec!["--model".to_string(), "sonnet".to_string()],
            "claude's own hard default stops a worker inheriting the operator's seat model"
        );
    }

    #[test]
    fn worker_model_args_adds_nothing_for_codex_with_no_configured_default() {
        let adapter = codex::CodexAdapter::new(None);
        let cfg = super::super::tests::permissive_cfg();
        assert_eq!(cfg.worker.codex, None, "nothing configured");
        assert!(
            worker_model_args(&cfg, "codex", &adapter).is_empty(),
            "codex has no adapter-owned default, so its own CLI/config default applies untouched"
        );
    }

    #[test]
    fn worker_model_args_uses_the_configured_codex_value_when_set() {
        let adapter = codex::CodexAdapter::new(None);
        let cfg = CtxConfig {
            worker: crate::commands::ctx::config::WorkerConfig {
                claude: None,
                codex: Some("gpt-5.6-terra".to_string()),
                ..Default::default()
            },
            ..super::super::tests::permissive_cfg()
        };
        assert_eq!(
            worker_model_args(&cfg, "codex", &adapter),
            vec!["--model".to_string(), "gpt-5.6-terra".to_string()],
        );
    }

    // FIX A: `last_model_flag` recognises codex's `-m` short alias in every
    // form, not just claude's long `--model`.
}
