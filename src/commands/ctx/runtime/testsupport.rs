//! Deterministic journal identities shared by the runtime modules' own
//! inline tests. `#[cfg(test)]` only: nothing here ships in the binary.

use super::super::provider::{
    AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
};
use super::journal::{RouteIdentity, SeatId, SessionIdentity};

/// One fixed native route. Every field is a literal, so a test that compares
/// two routes is comparing what it wrote, not what the machine is configured
/// with.
pub fn route_identity() -> RouteIdentity {
    RouteIdentity {
        route: RouteId::new("fixture").expect("static route id"),
        provider: ProviderId::new("anthropic").expect("static provider id"),
        endpoint: EndpointId::new("fixture").expect("static endpoint id"),
        account: AccountId::new("fixture").expect("static account id"),
        billing_pool: BillingPoolId::new("fixture").expect("static pool id"),
        protocol: Protocol::AnthropicMessages,
        model: ModelId {
            vendor: "fixture".into(),
            id: "fixture-model".into(),
        },
    }
}

pub fn session_identity(session: &str, route: RouteIdentity) -> SessionIdentity {
    SessionIdentity {
        session: super::journal::JournalSessionId::new(session).expect("session id"),
        seat: SeatId::new("seat-1").expect("seat id"),
        generation: 1,
        task: None,
        route,
        created_at: 1,
        completed_at: None,
    }
}
