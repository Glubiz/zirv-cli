//! The dashboard's link to the persistent runtime (issue #489, step N20; the
//! ownership half of issue #352's "the dashboard is still its own PTY owner"
//! residual).
//!
//! With `[session] persistent` on and a runtime listening, the terminals and
//! the conversations belong to the SERVICE. This module is the one seam
//! through which the dashboard reaches them: session facts, the attachment
//! table, a rendered screen, and a native conversation's durable event cursor,
//! all over the ordinary protocol v1 client. There is no second wire here, no
//! private frame and no direct call into `session::host`.
//!
//! Two things are deliberately NOT here:
//!
//! - **Presentation.** Layout, focus, drafts, scroll and colour stay in the
//!   dashboard, exactly as issue #489's item 4 requires; nothing in this
//!   module reads or writes any of it, and no protocol method could.
//! - **A pty.** [`RuntimeLink`] never opens a terminal, never spawns a child
//!   and never files a registry record. That is the whole point: when the
//!   runtime owns a session, the dashboard is a client of it.
//!
//! The ownership half is wired: `run_dashboard` asks this module whether the
//! runtime already holds this repository's seat, and refuses to open a second
//! terminal over it rather than becoming a competing supervisor. The RENDERING
//! half -- painting a runtime-owned session inside a dashboard pane, through
//! [`RuntimeLink::screen`] and [`RuntimeLink::events`] -- is step N11 (#480),
//! which owns pane rendering and is being built in parallel. The transport is
//! finished and tested here so that step is a pane change rather than a
//! protocol change; `#![allow(dead_code)]` covers the methods it will call,
//! the same reasoning `runtime/mod.rs` and `api/client.rs` already document
//! for their own untouched-but-tested surfaces.
#![allow(dead_code)]

use serde_json::json;

use super::super::CtxResult;
use super::super::api::client::Client;
use super::super::api::transport;
use super::super::api::wire::{
    ApprovalDecision, Attachment, Capability, Method, NativePage, ScreenView, SessionFacts,
    SessionState,
};
use super::super::runtime::RuntimeKind;
use super::super::state::StateDir;

/// Who owns a session's terminal or conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// This process opens the pty and holds the registry record -- everything
    /// the dashboard did before the persistent runtime existed.
    Dashboard,
    /// The runtime service owns it; the dashboard is a protocol client.
    Runtime,
}

/// Pure: who owns the sessions this dashboard is about to show.
///
/// Both conditions are required, and each for its own reason. The gate is
/// operator-only and off by default, so an operator who has not opted in is
/// never quietly turned into a client of a service they did not ask for. And a
/// gate that is on with nothing listening must NOT strand the dashboard: the
/// service may be stopped, mid-restart or refusing a namespace, and a
/// dashboard that owned no terminals in that state would simply not work.
pub fn ownership(persistent: bool, runtime_listening: bool) -> Ownership {
    if persistent && runtime_listening {
        Ownership::Runtime
    } else {
        Ownership::Dashboard
    }
}

/// The refusal the dashboard prints rather than opening a second terminal for
/// a session the runtime already holds. Named once so the wording cannot
/// drift between the surfaces that use it.
pub const RUNTIME_OWNS_IT: &str = "the persistent runtime already owns a session for this repository: attach to it with \
     `zirv session attach` (or `zirv chat`). Opening a second terminal here would leave two \
     supervisors on one conversation, which is exactly what the runtime exists to prevent.";

/// A connected dashboard client of the runtime.
#[derive(Debug)]
pub struct RuntimeLink {
    client: Client,
    client_id: String,
}

impl RuntimeLink {
    /// Connects, or answers `None` when the operator has not opted in or
    /// nothing is listening. Never an error: a dashboard that could not reach
    /// a runtime owns its own terminals, which is a working mode rather than
    /// a failure.
    pub fn connect(state: &StateDir, persistent: bool) -> Option<Self> {
        let endpoint = super::super::api::server::endpoint_for(state);
        if ownership(persistent, transport::probe(&endpoint)) != Ownership::Runtime {
            return None;
        }
        let client = Client::connect(&endpoint).ok()?;
        Some(Self {
            client,
            // The same shape `session::client::client_id` mints, and for the
            // same reason: stable for the life of the process, so a dashboard
            // that reconnects takes its own place back rather than
            // accumulating ghost attachments, and never reused across
            // processes because the pid is in it.
            client_id: super::super::session::client::client_id("dash"),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Whether the runtime on the other end owns native conversations as well
    /// as terminals. Negotiated locally from the `hello` frame: a feature the
    /// server never advertised is disabled here, not attempted and refused.
    pub fn serves_native(&self) -> bool {
        self.client.negotiated().has(Capability::SessionNative)
    }

    /// Every session the runtime holds, live ones first.
    pub fn sessions(&mut self) -> CtxResult<Vec<SessionFacts>> {
        let value = self.client.call(Method::SessionSnapshot, json!({}))?;
        let sessions = value.get("sessions").cloned().unwrap_or_else(|| json!([]));
        Ok(serde_json::from_value(sessions)?)
    }

    /// The live session the runtime holds for `repo_slug` and `agent`, if any.
    /// The ownership question the dashboard actually asks at startup: is there
    /// already a supervisor for the seat I was about to open?
    pub fn seat_for(&mut self, repo_slug: &str, agent: &str) -> CtxResult<Option<SessionFacts>> {
        Ok(self.sessions()?.into_iter().find(|facts| {
            facts.state != SessionState::Ended
                && facts.repo_slug.as_deref() == Some(repo_slug)
                && facts.agent.as_deref() == Some(agent)
        }))
    }

    /// Attaches this dashboard to a runtime session. An observer by default,
    /// deliberately: an attachment that silently seized the keyboard from
    /// whoever was already typing would be the opposite of "takeover is
    /// explicit and visible".
    pub fn attach(&mut self, session_id: &str, controller: bool) -> CtxResult<Attachment> {
        let value = self.client.call(
            Method::SessionAttach,
            json!({
                "session_id": session_id,
                "client_id": self.client_id,
                "mode": if controller { "controller" } else { "observer" },
            }),
        )?;
        Ok(serde_json::from_value(
            value
                .get("attachment")
                .cloned()
                .unwrap_or_else(|| json!({})),
        )?)
    }

    /// Releases this dashboard's own attachment. The session, its process or
    /// its journal, its supervisor and its registry record are untouched --
    /// this is the method that makes closing the dashboard cost a repaint.
    pub fn detach(&mut self, session_id: &str) -> CtxResult<()> {
        self.client.call(
            Method::SessionDetach,
            json!({"session_id": session_id, "client_id": self.client_id}),
        )?;
        Ok(())
    }

    /// Terminates a runtime-owned session. Unlike [`Self::detach`], this
    /// addresses the session itself rather than this dashboard's attachment.
    pub fn stop(&mut self, session_id: &str) -> CtxResult<bool> {
        let value = self
            .client
            .call(Method::SessionStop, json!({"session_id": session_id}))?;
        value
            .get("stopped")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| "runtime link: session.stop response omitted boolean stopped".into())
    }

    /// The rendered terminal of a runtime-owned pty session, for a dashboard
    /// that is painting it rather than owning it.
    pub fn screen(&mut self, session_id: &str) -> CtxResult<ScreenView> {
        let value = self.client.call(
            Method::SessionScreen,
            json!({"session_id": session_id, "client_id": self.client_id}),
        )?;
        Ok(serde_json::from_value(
            value.get("screen").cloned().unwrap_or_else(|| json!({})),
        )?)
    }

    /// One bounded page of a native conversation's durable event stream. The
    /// native counterpart of [`Self::screen`]: a conversation has no cells to
    /// paint, it has events to render, and the cursor is what lets a
    /// reconnecting dashboard carry on where it left off instead of
    /// re-reading an hour of journal.
    pub fn events(&mut self, session_id: &str, after: u64) -> CtxResult<NativePage> {
        let value = self.client.call(
            Method::SessionJournal,
            json!({
                "session_id": session_id,
                "client_id": self.client_id,
                "after_sequence": after,
            }),
        )?;
        Ok(serde_json::from_value(
            value.get("page").cloned().unwrap_or_else(|| json!({})),
        )?)
    }

    /// The operator's literal keystrokes, for a runtime-owned TERMINAL. Only
    /// the controller may send these, and the runtime is what enforces that.
    pub fn type_into(&mut self, session_id: &str, bytes: &str) -> CtxResult<()> {
        self.client.call(
            Method::SessionSendInput,
            json!({
                "session_id": session_id,
                "client_id": self.client_id,
                "mode": "raw",
                "input": bytes,
            }),
        )?;
        Ok(())
    }

    /// A turn for a runtime-owned CONVERSATION. `idempotency` is the
    /// dashboard's own retry identity: a reconnect that resends the same
    /// submission must not start a second turn, and the runtime settles that
    /// against durable state rather than against a cache.
    pub fn submit(
        &mut self,
        session_id: &str,
        text: &str,
        idempotency: Option<&str>,
    ) -> CtxResult<()> {
        self.client.call_with_key(
            Method::SessionSendInput,
            json!({
                "session_id": session_id,
                "client_id": self.client_id,
                "mode": "submit",
                "input": text,
            }),
            idempotency,
        )?;
        Ok(())
    }

    /// Ends the TURN of a runtime-owned conversation, never the session --
    /// the native counterpart of the dashboard's own `Esc`. Only the
    /// controller may send it, and the runtime is what enforces that.
    pub fn interrupt(&mut self, session_id: &str) -> CtxResult<()> {
        self.client.call(
            Method::SessionInterrupt,
            json!({
                "session_id": session_id,
                "client_id": self.client_id,
            }),
        )?;
        Ok(())
    }

    /// Issue #490: the operator's answer to an approval a runtime-owned
    /// conversation is blocked on. The decision is delivered to the session
    /// that actually asked -- the dashboard never mints a grant of its own,
    /// and `request_id` is the runtime's own identifier for the outstanding
    /// request, so a stale dialog cannot answer a newer question. `note` is
    /// the "tell the agent what to do differently" text a denial carries.
    pub fn approve(
        &mut self,
        session_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
        note: Option<&str>,
    ) -> CtxResult<()> {
        self.client.call(
            Method::SessionApprove,
            json!({
                "session_id": session_id,
                "client_id": self.client_id,
                "request_id": request_id,
                "decision": decision,
                "note": note,
            }),
        )?;
        Ok(())
    }
}

/// Which transport a session's content comes over, given the facts the runtime
/// published for it. Pure, so the dashboard's rendering choice is decided by
/// the session's own backend rather than by whichever call happened to fail.
pub fn content_source(facts: &SessionFacts) -> ContentSource {
    match facts.runtime {
        RuntimeKind::Native => ContentSource::Events,
        // `Unknown` is a runtime a newer build wrote that this one has never
        // heard of. A screen is the older, more universal shape, and asking
        // for one is a refusal at worst -- guessing `Events` would mean
        // rendering nothing at all.
        RuntimeKind::Harness | RuntimeKind::Unknown => ContentSource::Screen,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentSource {
    /// `session.screen`: rendered terminal cells.
    Screen,
    /// `session.journal`: durable native events, by cursor.
    Events,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::api::server::{ApiServer, RunningServer, StaticSource};
    use crate::commands::ctx::runtime::UiSurface;
    use std::sync::Arc;

    fn facts(id: &str, runtime: RuntimeKind) -> SessionFacts {
        SessionFacts {
            session_id: id.to_string(),
            short: crate::commands::ctx::sessions::short_id(id),
            runtime,
            generation: 1,
            surface: UiSurface::Headless,
            state: SessionState::Idle,
            role: Some("orchestrator".to_string()),
            agent: Some("claude".to_string()),
            repo_slug: Some("zirv-cli".to_string()),
            started_at: Some(1_757_000_000),
            reachable: true,
        }
    }

    /// Issue #352's residual as a rule: with the gate on and a runtime
    /// listening, the dashboard is a CLIENT. Both halves are required, and a
    /// gate with nothing listening must leave the dashboard working rather
    /// than stranded.
    #[test]
    fn the_dashboard_owns_terminals_only_when_no_runtime_does() {
        assert_eq!(ownership(true, true), Ownership::Runtime);
        assert_eq!(
            ownership(true, false),
            Ownership::Dashboard,
            "a gate with nothing listening must not strand the dashboard"
        );
        assert_eq!(
            ownership(false, true),
            Ownership::Dashboard,
            "an operator who never opted in is never quietly made a client"
        );
        assert_eq!(ownership(false, false), Ownership::Dashboard);
    }

    /// A conversation is rendered from its durable events and a terminal from
    /// its screen, decided by the session's own backend -- and an unfamiliar
    /// backend falls back to the older, more universal shape rather than to
    /// rendering nothing.
    #[test]
    fn content_comes_from_the_sessions_own_backend() {
        assert_eq!(
            content_source(&facts("s1", RuntimeKind::Harness)),
            ContentSource::Screen
        );
        assert_eq!(
            content_source(&facts("s2", RuntimeKind::Native)),
            ContentSource::Events
        );
        assert_eq!(
            content_source(&facts("s3", RuntimeKind::Unknown)),
            ContentSource::Screen
        );
    }

    /// `connect` answers `None` rather than failing when the operator has not
    /// opted in or nothing is listening: owning your own terminals is a
    /// working mode, not an error.
    #[test]
    fn connecting_without_a_runtime_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(RuntimeLink::connect(&state, true).is_none());
        assert!(RuntimeLink::connect(&state, false).is_none());
    }

    /// Over the real transport: the dashboard reads the runtime's session
    /// facts through the published protocol and nothing else. It finds the
    /// seat it would otherwise have opened a second terminal for, and it
    /// learns which surface that session's content comes over -- all without
    /// a pty, a child process or a registry write of its own.
    #[test]
    fn the_dashboard_reads_the_runtimes_sessions_over_the_protocol() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let endpoint = super::super::super::api::server::endpoint_for(&state);
        let server = ApiServer::new(
            Box::new(StaticSource(vec![
                facts("aaaaaaaa-0000-4000-8000-000000000001", RuntimeKind::Harness),
                facts("bbbbbbbb-0000-4000-8000-000000000002", RuntimeKind::Native),
            ])),
            None,
        );
        let running = RunningServer::start(&endpoint, Arc::clone(&server)).expect("start");

        let mut link = RuntimeLink::connect(&state, true).expect("a runtime is listening");
        assert!(
            link.client_id().starts_with("dash-"),
            "the dashboard identifies itself: {}",
            link.client_id()
        );
        let sessions = link.sessions().expect("sessions");
        assert_eq!(sessions.len(), 2);

        let seat = link
            .seat_for("zirv-cli", "claude")
            .expect("seat lookup")
            .expect("the runtime already holds a seat for this repository");
        assert_eq!(seat.repo_slug.as_deref(), Some("zirv-cli"));
        assert_eq!(
            content_source(&seat),
            ContentSource::Screen,
            "a harness seat is painted from its screen"
        );
        assert!(
            !link.serves_native(),
            "this server owns no conversations, so the dashboard disables that surface locally"
        );

        drop(link);
        drop(running);
    }
}
