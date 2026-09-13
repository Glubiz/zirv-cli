//! The native team: who a seat is, which route it spends, and what that
//! identity alone entitles it to (issue #485, roadmap N16).
//!
//! A meta-orchestrator that runs inside zirv rather than inside someone
//! else's harness needs three things this module owns, and nothing else:
//!
//! 1. **A closed set of team roles.** `coordinator`, `sub-orchestrator`,
//!    `researcher`, `planner`, `implementer`, `reviewer`, `tester`. They are
//!    a set rather than free text because two of the decisions below are made
//!    FROM the role, and a decision made from an unvalidated string is a
//!    decision a typo can change.
//! 2. **Explicit role-to-route selection.** The operator's `[roles]` table in
//!    `~/.zirv/native.toml` is the whole mechanism -- the same table
//!    `zirv ctx exec --runtime native`, `zirv agent --runtime native` and
//!    `ctx::helper` already read. A role with no entry is a TYPED REFUSAL
//!    naming the roles that do have one. There is deliberately no fallback
//!    chain: answering "no route for `reviewer`" with the coordinator's own
//!    expensive route would be inferring an entitlement nobody granted.
//! 3. **Authority from the role, not from the caller.** [`Authority`] is a
//!    pure function of the role zirv itself minted into the seat record
//!    (`seat::Seat::role`, reachable at effect time as
//!    `ExecutionIdentity::role`). Model output never supplies it, and a
//!    parent's own narrowed envelope never shrinks it: a coordinator that is
//!    itself read-only still delegates a WRITING implementer, and a reviewer
//!    seat still may not delegate at all however writable its parent was.
//!
//! Route eligibility reuses N18's model wholesale ([`super::route::eligible`]
//! over [`super::route::offers_from_config`]): a model-chosen route for a
//! role is admitted only when it clears operator policy AND is billed the way
//! the role's own configured route is billed. Moving a subscription-seated
//! role onto metered API credit is a billing decision, so it is refused with
//! N18's own `UnauthorizedBilling` reason rather than taken quietly.

use std::collections::BTreeSet;

use super::prompt::PromptRole;
use super::provider::RouteId;
use super::provider::config::NativeConfig;
use super::route::{self, BillingPosture, Demand, Ineligible};

pub const COORDINATOR: &str = "coordinator";
pub const SUB_ORCHESTRATOR: &str = "sub-orchestrator";
pub const RESEARCHER: &str = "researcher";
pub const PLANNER: &str = "planner";
pub const IMPLEMENTER: &str = "implementer";
pub const REVIEWER: &str = "reviewer";
pub const TESTER: &str = "tester";

/// The role `zirv ctx exec --runtime native` and `zirv agent --runtime
/// native` have always used when `--role` is absent. Not a team role: it
/// carries the least-privileged worker methodology and no delegation
/// authority, which is the right default for a session nobody named.
pub const DEFAULT_ROLE: &str = "worker";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeamRole {
    Coordinator,
    SubOrchestrator,
    Researcher,
    Planner,
    Implementer,
    Reviewer,
    Tester,
}

/// Every team role, in the order a feature actually moves through them. The
/// one list, so the parser, the authority table and the diagnostics cannot
/// drift apart.
pub const TEAM: [TeamRole; 7] = [
    TeamRole::Coordinator,
    TeamRole::SubOrchestrator,
    TeamRole::Researcher,
    TeamRole::Planner,
    TeamRole::Implementer,
    TeamRole::Reviewer,
    TeamRole::Tester,
];

impl TeamRole {
    /// `None` for anything outside the team -- `worker`, `seat`, `ask`, an
    /// operator's own label. Deliberately not a fallback to some "closest"
    /// role: an unrecognised role gets worker authority and worker
    /// methodology, never a coordinator's.
    ///
    /// `orchestrator` is accepted as a spelling of `coordinator`: it is the
    /// name the prompt layer (`PromptRole::Orchestrator`) and every seat
    /// record written before this step already use for the same seat.
    pub fn parse(role: &str) -> Option<Self> {
        match role {
            COORDINATOR | "orchestrator" => Some(Self::Coordinator),
            SUB_ORCHESTRATOR => Some(Self::SubOrchestrator),
            RESEARCHER => Some(Self::Researcher),
            PLANNER => Some(Self::Planner),
            IMPLEMENTER => Some(Self::Implementer),
            REVIEWER => Some(Self::Reviewer),
            TESTER => Some(Self::Tester),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => COORDINATOR,
            Self::SubOrchestrator => SUB_ORCHESTRATOR,
            Self::Researcher => RESEARCHER,
            Self::Planner => PLANNER,
            Self::Implementer => IMPLEMENTER,
            Self::Reviewer => REVIEWER,
            Self::Tester => TESTER,
        }
    }

    /// Which methodology layer the native context compiler injects for this
    /// role. Only the two coordinating roles get an orchestrator layer; a
    /// researcher, planner, implementer, reviewer or tester is a worker that
    /// was handed one scope, and telling it to fan work out would invite the
    /// recursion the depth cap exists to stop.
    pub fn prompt_role(self) -> PromptRole {
        match self {
            Self::Coordinator => PromptRole::Orchestrator,
            Self::SubOrchestrator => PromptRole::SubOrchestrator,
            _ => PromptRole::Worker,
        }
    }

    pub fn authority(self) -> Authority {
        match self {
            Self::Coordinator => Authority {
                may_delegate: true,
                may_write: true,
            },
            // A sub-orchestrator splits one scope and dispatches workers for
            // it; the depth cap (`envelope::WorkerEnvelope::delegation_depth`)
            // is what stops the chain, not this flag.
            Self::SubOrchestrator => Authority {
                may_delegate: true,
                may_write: true,
            },
            // A planner and a researcher produce findings and plans. Neither
            // needs a checkout, and a seat that cannot write cannot leave a
            // half-applied change behind when it is cancelled.
            Self::Researcher | Self::Planner => Authority {
                may_delegate: false,
                may_write: false,
            },
            Self::Implementer => Authority {
                may_delegate: false,
                may_write: true,
            },
            // The two verification roles are read-only by identity. A
            // reviewer that may edit the thing it is reviewing is not a
            // review, and a tester's evidence has to describe a tree it did
            // not change.
            Self::Reviewer | Self::Tester => Authority {
                may_delegate: false,
                may_write: false,
            },
        }
    }
}

impl std::fmt::Display for TeamRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a seat holding a role may do, derived from that role ALONE.
///
/// Issue #485 item 6. Two failure modes this exists to prevent, in both
/// directions:
///
/// - **over-restriction by inheritance**: a coordinator running read-only
///   (a `--mode read-only` seat, a helper) must still be able to delegate an
///   implementer that writes. The child's authority is its own role's, not a
///   copy of the parent's posture.
/// - **escalation by request**: a reviewer seat must not delegate, and a
///   reviewer CHILD must not write, no matter what the delegating model
///   asked for. The role is read off the persisted seat record, which model
///   output cannot reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authority {
    pub may_delegate: bool,
    pub may_write: bool,
}

impl Authority {
    /// Authority for any role outside the team: the least-privileged answer.
    /// An unknown role is not evidence of an entitlement.
    pub const fn worker() -> Self {
        Self {
            may_delegate: false,
            may_write: true,
        }
    }
}

/// The authority a role string carries. Outside the team this is
/// [`Authority::worker`] -- unchanged from every pre-N16 delegation, which
/// is what keeps `worker`, `seat` and an operator's own labels working
/// exactly as they did.
pub fn authority(role: &str) -> Authority {
    TeamRole::parse(role)
        .map(TeamRole::authority)
        .unwrap_or_else(Authority::worker)
}

/// Which methodology a role string gets. The one place `--role` becomes a
/// [`PromptRole`], shared by the native session and by anything that has to
/// predict what a seat will be told.
pub fn prompt_role(role: &str) -> PromptRole {
    TeamRole::parse(role)
        .map(TeamRole::prompt_role)
        .unwrap_or(PromptRole::Worker)
}

/// Why a role could not be given a route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteRefusal {
    /// No `[roles]` entry for this role. The ordinary, expected case on a
    /// machine that configured a subset of the team, and reported as a fact
    /// about configuration rather than as a malfunction.
    Unconfigured {
        role: String,
        configured: Vec<String>,
    },
    /// A route was named that the operator's configuration does not declare.
    Unknown { role: String, route: String },
    /// The route exists but may not take this role's work: operator policy
    /// refuses it, it lacks a capability, or it is billed a way this role is
    /// not authorized for. N18's own reason text, unchanged.
    Ineligible {
        role: String,
        route: String,
        reason: Ineligible,
    },
}

impl std::fmt::Display for RouteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unconfigured { role, configured } => write!(
                f,
                "no route for role `{role}`; add a [roles] entry (configured roles: {})",
                if configured.is_empty() {
                    "none".to_string()
                } else {
                    configured.join(", ")
                }
            ),
            Self::Unknown { role, route } => write!(
                f,
                "role `{role}`: route `{route}` is not declared in the native provider \
                 configuration"
            ),
            Self::Ineligible {
                role,
                route,
                reason,
            } => write!(f, "role `{role}`: route `{route}` {}", reason.label()),
        }
    }
}

impl std::error::Error for RouteRefusal {}

fn configured_roles(config: &NativeConfig) -> Vec<String> {
    config.roles.keys().cloned().collect()
}

/// The route the operator configured for `role`.
///
/// The whole of item 2's selection mechanism: one lookup in the operator's
/// own `[roles]` table, with a typed refusal when there is no entry. No
/// inference, no nearest-neighbour, no reuse of another role's route.
pub fn route_for_role(config: &NativeConfig, role: &str) -> Result<RouteId, RouteRefusal> {
    config
        .roles
        .get(role)
        .cloned()
        .ok_or_else(|| RouteRefusal::Unconfigured {
            role: role.to_string(),
            configured: configured_roles(config),
        })
}

/// Every configured route with its id, in configuration order.
///
/// [`route::offers_from_config`] maps `config.routes` in place, so zipping
/// the keys back on is exact rather than a lookup by name -- and it is the
/// only way to get an id back, since a `RouteOffer`'s identity names the
/// account, endpoint and pool a route resolves to rather than the route id
/// itself.
fn offers_by_id(config: &NativeConfig) -> Vec<(RouteId, route::RouteOffer)> {
    config
        .routes
        .keys()
        .cloned()
        .zip(route::offers_from_config(config))
        .collect()
}

/// The billing posture the operator's own configuration puts `role` on.
/// `None` when the role has no configured route at all, which is the case
/// [`route_for_role`] refuses separately.
fn configured_posture(config: &NativeConfig, role: &str) -> Option<BillingPosture> {
    let configured = config.roles.get(role)?;
    offers_by_id(config)
        .into_iter()
        .find(|(id, _)| id == configured)
        .map(|(_, offer)| offer.billing)
}

/// Admit a REQUESTED route for `role` -- the route a delegating model named
/// rather than the one the operator configured.
///
/// Three gates, in the order they matter: the route has to exist, it has to
/// clear operator policy and capability (N18's [`route::eligible`]), and it
/// has to be billed the way the role's own configured route is billed. The
/// last one is the "provider changes follow configured policy" half of item
/// 2: an operator who seated `reviewer` on a subscription did not thereby
/// authorize a model to move that work onto metered API credit, and the
/// refusal says so instead of quietly re-routing.
///
/// A role with no configured route of its own states no billing constraint,
/// so the requested route is judged on policy and capability alone -- there
/// is nothing to change the billing away FROM.
pub fn authorize_route(
    config: &NativeConfig,
    role: &str,
    requested: &str,
) -> Result<RouteId, RouteRefusal> {
    let requested_id = RouteId::new(requested).map_err(|_| RouteRefusal::Unknown {
        role: role.to_string(),
        route: requested.to_string(),
    })?;
    let offer = offers_by_id(config)
        .into_iter()
        .find(|(id, _)| *id == requested_id)
        .map(|(_, offer)| offer)
        .ok_or_else(|| RouteRefusal::Unknown {
            role: role.to_string(),
            route: requested.to_string(),
        })?;
    let mut authorized: BTreeSet<BillingPosture> = BTreeSet::new();
    if let Some(posture) = configured_posture(config, role) {
        authorized.insert(posture);
    }
    let demand = Demand {
        authorized_billing: authorized,
        preferred: config.roles.get(role).map(RouteId::to_string),
        ..Demand::default()
    };
    match route::eligible(&offer, &demand) {
        Ok(()) => Ok(requested_id),
        Err(reason) => Err(RouteRefusal::Ineligible {
            role: role.to_string(),
            route: requested.to_string(),
            reason,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NativeConfig::load` resolves an absent `policy.allowed_routes` to
    /// "every declared route" before it validates, and `allowed_routes()`
    /// relies on that having happened. A fixture parsed straight from text
    /// has to do the same, or it is not the shape production ever sees.
    fn config(toml: &str) -> NativeConfig {
        let mut config: NativeConfig = toml::from_str(toml).expect("native config fixture");
        if config.policy.allowed_routes.is_none() {
            config.policy.allowed_routes = Some(config.routes.keys().cloned().collect());
        }
        config
    }

    /// One account on a subscription and one on metered API credit, with a
    /// role seated on each -- the shape item 2's billing rule is about.
    fn two_postures() -> NativeConfig {
        config(
            "schema = 1\n\
             [account.seat]\nprovider='anthropic'\ncredential='env:A'\nbilling='subscription'\n\
             [account.metered]\nprovider='anthropic'\ncredential='env:B'\nbilling='api'\n\
             [route.seated]\naccount='seat'\nmodel='claude-sonnet-4-5'\n\
             [route.paid]\naccount='metered'\nmodel='claude-sonnet-4-5'\n\
             [roles]\ncoordinator='seated'\nimplementer='paid'\n",
        )
    }

    #[test]
    fn the_team_is_a_closed_set_and_orchestrator_is_the_coordinators_other_name() {
        for role in TEAM {
            assert_eq!(TeamRole::parse(role.as_str()), Some(role), "{role}");
        }
        assert_eq!(TeamRole::parse("orchestrator"), Some(TeamRole::Coordinator));
        for outside in ["worker", "seat", "ask", "coordinatorr", "Coordinator", ""] {
            assert_eq!(TeamRole::parse(outside), None, "{outside:?}");
        }
    }

    #[test]
    fn only_the_coordinating_roles_get_an_orchestrator_methodology() {
        assert_eq!(prompt_role(COORDINATOR), PromptRole::Orchestrator);
        assert_eq!(prompt_role("orchestrator"), PromptRole::Orchestrator);
        assert_eq!(prompt_role(SUB_ORCHESTRATOR), PromptRole::SubOrchestrator);
        for worker_role in [RESEARCHER, PLANNER, IMPLEMENTER, REVIEWER, TESTER, "worker"] {
            assert_eq!(
                prompt_role(worker_role),
                PromptRole::Worker,
                "{worker_role} must not be told to fan work out"
            );
        }
    }

    /// Issue #485 item 6, both directions in one place: authority is a
    /// function of the role alone, so it neither shrinks to match a
    /// restricted parent nor grows because a caller asked.
    #[test]
    fn authority_comes_from_the_role_and_nothing_else() {
        assert!(authority(COORDINATOR).may_delegate);
        assert!(authority(COORDINATOR).may_write);
        assert!(authority(SUB_ORCHESTRATOR).may_delegate);

        assert!(
            authority(IMPLEMENTER).may_write,
            "an implementer writes even when whoever delegated it could not"
        );
        assert!(
            !authority(IMPLEMENTER).may_delegate,
            "and it still may not fan work out"
        );

        for read_only in [REVIEWER, TESTER, RESEARCHER, PLANNER] {
            assert!(!authority(read_only).may_write, "{read_only}");
            assert!(!authority(read_only).may_delegate, "{read_only}");
        }

        // Outside the team nothing changes: this is what every pre-N16
        // `worker` delegation already got.
        assert_eq!(authority("worker"), Authority::worker());
        assert_eq!(authority("something-new"), Authority::worker());
    }

    #[test]
    fn a_role_with_no_entry_is_a_typed_refusal_naming_the_roles_that_have_one() {
        let cfg = two_postures();
        assert_eq!(
            route_for_role(&cfg, COORDINATOR).expect("configured"),
            RouteId::new("seated").expect("id")
        );
        let refusal = route_for_role(&cfg, REVIEWER).expect_err("no reviewer route");
        let RouteRefusal::Unconfigured { role, configured } = &refusal else {
            panic!("expected an unconfigured refusal, got {refusal:?}");
        };
        assert_eq!(role, REVIEWER);
        assert_eq!(configured, &["coordinator".to_string(), "implementer".into()]);
        // The refusal names what IS configured rather than silently handing
        // the reviewer somebody else's route.
        assert!(refusal.to_string().contains("coordinator"));
    }

    #[test]
    fn a_requested_route_may_not_move_a_role_onto_different_billing() {
        let cfg = two_postures();
        // The role's own route is always admissible.
        assert_eq!(
            authorize_route(&cfg, COORDINATOR, "seated").expect("its own route"),
            RouteId::new("seated").expect("id")
        );
        // A sibling route on the SAME subscription account would be fine;
        // the metered one is a billing change and is refused with N18's own
        // reason rather than taken.
        let refusal = authorize_route(&cfg, COORDINATOR, "paid").expect_err("billing change");
        let RouteRefusal::Ineligible { reason, .. } = &refusal else {
            panic!("expected an eligibility refusal, got {refusal:?}");
        };
        assert!(
            matches!(reason, Ineligible::UnauthorizedBilling { .. }),
            "got {reason:?}"
        );
        assert!(refusal.to_string().contains("subscription"), "{refusal}");
    }

    #[test]
    fn an_undeclared_route_is_refused_before_any_eligibility_question() {
        let cfg = two_postures();
        for bogus in ["nope", "../escape", "Paid", ""] {
            let refusal = authorize_route(&cfg, COORDINATOR, bogus).expect_err("{bogus}");
            assert!(
                matches!(refusal, RouteRefusal::Unknown { .. }),
                "{bogus:?} -> {refusal:?}"
            );
        }
    }

    #[test]
    fn a_role_with_no_configured_route_states_no_billing_constraint() {
        let cfg = two_postures();
        // `reviewer` is unconfigured, so there is no posture to change AWAY
        // from: the request is judged on policy and capability alone.
        assert_eq!(
            authorize_route(&cfg, REVIEWER, "paid").expect("no constraint to violate"),
            RouteId::new("paid").expect("id")
        );
    }

    #[test]
    fn operator_policy_outranks_the_role_table() {
        let cfg = config(
            "schema = 1\n\
             [account.seat]\nprovider='anthropic'\ncredential='env:A'\n\
             [route.a]\naccount='seat'\nmodel='claude-sonnet-4-5'\n\
             [route.b]\naccount='seat'\nmodel='claude-sonnet-4-5'\n\
             [roles]\ncoordinator='a'\n\
             [policy]\nallowed_routes=['a']\n",
        );
        let refusal = authorize_route(&cfg, COORDINATOR, "b").expect_err("outside policy");
        let RouteRefusal::Ineligible { reason, .. } = &refusal else {
            panic!("expected an eligibility refusal, got {refusal:?}");
        };
        assert!(
            matches!(reason, Ineligible::PolicyRefused { .. }),
            "got {reason:?}"
        );
    }
}
