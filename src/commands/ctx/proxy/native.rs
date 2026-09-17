//! Bridges a [`ProxyDecision`]'s orchestrator seat onto the native runtime's
//! own route table (issue #537 seam). One guarded, pure lookup: the native
//! gate, `NATIVE_COMING_SOON` and the `--runtime native` refusal all stay in
//! `runtime/mod.rs`, untouched by this module.

use super::decision::ProxyDecision;
use crate::commands::ctx::adapters;
use crate::commands::ctx::catalogue;
use crate::commands::ctx::provider::RouteId;
use crate::commands::ctx::provider::config::NativeConfig;

/// The configured route whose `model` equals the decision's own orchestrator
/// model, matched against either the alias the decision carries (e.g.
/// `"fable"`) or the canonical catalogue id it resolves to on that harness's
/// vendor (e.g. `"claude-fable-5"`). `None` when no route matches -- the
/// caller keeps its configured role route in that case, exactly as the
/// spec's "Apply -- native runtime" section describes.
// T2 is the first caller (`spawn_interactive`'s submit loop); exercised
// here only by this module's own tests in the meantime.
#[allow(dead_code)]
pub fn route_for_decision(decision: &ProxyDecision, cfg: &NativeConfig) -> Option<RouteId> {
    let vendor_slug = adapters::provider_for_agent_name(Some(&decision.orchestrator.harness));
    let canonical_id = catalogue::vendor(vendor_slug)
        .and_then(|vendor| catalogue::rung_of(vendor, &decision.orchestrator.model))
        .map(|rung| rung.id);

    cfg.routes
        .iter()
        .find(|(_, route)| {
            route.model == decision.orchestrator.model || Some(route.model.as_str()) == canonical_id
        })
        .map(|(id, _)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::proxy::decision::{Decider, Seat};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn decision(harness: &str, model: &str) -> ProxyDecision {
        ProxyDecision {
            request_sha256: "x".repeat(64),
            repo: PathBuf::from("/tmp/repo"),
            intent: crate::commands::workflow::classify::Intent::Feature,
            complexity: crate::commands::workflow::classify::Complexity::Bounded,
            risk: crate::commands::workflow::classify::RiskBand::Low,
            execution: crate::commands::workflow::profile::ExecutionMode::Bounded,
            validation: crate::commands::workflow::profile::ValidationProfile::default(),
            workflow: None,
            orchestrator: Seat {
                harness: harness.to_string(),
                model: model.to_string(),
            },
            worker_tier: catalogue::Tier::Standard,
            needs_clarification: 0.0,
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 0,
            usage: None,
            created_at: 0,
        }
    }

    /// Builds a `NativeConfig` in-test the same way `provider/config.rs`'s
    /// own tests do: parse a minimal TOML fragment rather than constructing
    /// the (many-field, `Default`-only-via-nested-defaults) struct by hand.
    fn native_config(routes_toml: &str) -> NativeConfig {
        toml::from_str(routes_toml).expect("parse native config fixture")
    }

    #[test]
    fn matches_the_route_whose_model_equals_the_decided_alias() {
        let cfg = native_config(
            r#"
            [route.work-sonnet]
            account = "acct"
            model = "sonnet"

            [route.other]
            account = "acct"
            model = "something-else"
            "#,
        );
        let route = route_for_decision(&decision("claude", "sonnet"), &cfg);
        assert_eq!(
            route.map(|id| id.to_string()),
            Some("work-sonnet".to_string())
        );
    }

    #[test]
    fn matches_the_route_whose_model_equals_the_canonical_id() {
        let cfg = native_config(
            r#"
            [route.work-sonnet]
            account = "acct"
            model = "claude-sonnet-5"
            "#,
        );
        let route = route_for_decision(&decision("claude", "sonnet"), &cfg);
        assert_eq!(
            route.map(|id| id.to_string()),
            Some("work-sonnet".to_string())
        );
    }

    #[test]
    fn no_match_returns_none() {
        let cfg = native_config(
            r#"
            [route.work-sonnet]
            account = "acct"
            model = "gpt-5.6-sol"
            "#,
        );
        assert_eq!(
            route_for_decision(&decision("claude", "sonnet"), &cfg),
            None
        );
    }
}
