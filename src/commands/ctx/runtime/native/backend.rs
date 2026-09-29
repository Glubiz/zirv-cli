//! [`NativeBackend`], the `RuntimeKind::Native` [`RuntimeBackend`] impl.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::super::super::CtxResult;
use super::super::super::provider::adapter::CancellationFlag;
use super::super::super::state::now_ms;
use super::super::journal::{EventScope, Journal, JournalSessionId, MessageId};
use super::super::{
    BackendConversationRef, RuntimeBackend, RuntimeCapabilities, RuntimeError, RuntimeKind,
    SessionHandle, SessionSpec, UiSurface,
};
use super::turn::{acknowledge_input, resume_journal};
use super::types::SessionState;

/// Durable input identity and whether the journal already held it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedInput {
    pub message_id: MessageId,
    pub duplicate: bool,
}

/// The journal identity one caller-chosen idempotency key maps to.
///
/// Hashed rather than used verbatim: a key is caller text, and a `MessageId`
/// is bounded, NUL-free and compared for equality. A cryptographic digest
/// keeps distinct keys distinct -- a cheap hash's collision would silently
/// drop a genuinely different input as a duplicate, which is the one failure
/// mode this whole mechanism exists to prevent.
pub fn idempotent_message_id(key: &str) -> CtxResult<MessageId> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(MessageId::new(format!("idem-{hex}"))?)
}

// -- the RuntimeBackend ---------------------------------------------------

/// The `RuntimeKind::Native` [`RuntimeBackend`].
///
/// The backend owns session identity, the durable acknowledgement of input
/// and the cancellation flag; [`NativeLoop`] owns the conversation.
///
/// A backend with a journal attached ([`NativeBackend::attach_journal`] plus
/// [`NativeBackend::bind_session`]) writes every accepted input through the
/// same [`acknowledge_input`] the loop uses, and resumes through the same
/// [`resume_journal`]. Without one it is a pure in-memory protocol surface,
/// which is all a wire-shape test needs and all `runtime::select` can build
/// without knowing a state directory.
#[derive(Debug)]
pub struct NativeBackend {
    pub(super) sessions: BTreeMap<String, NativeSessionRecord>,
    journal: Option<Journal>,
    minted: u64,
}

#[derive(Debug)]
pub(super) struct NativeSessionRecord {
    short: String,
    generation: u64,
    role: String,
    surface: UiSurface,
    pub(super) state: SessionState,
    cancel: Arc<CancellationFlag>,
    events: Vec<super::super::protocol::EventEnvelope>,
    /// The journal session this handle's inputs are recorded against, once a
    /// caller has bound one. `None` for an unbacked in-memory session.
    journal_session: Option<JournalSessionId>,
}

impl NativeBackend {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            journal: None,
            minted: 0,
        }
    }

    /// Gives this backend the journal every accepted input is recorded in.
    pub fn attach_journal(&mut self, journal: Journal) {
        self.journal = Some(journal);
    }

    /// [`RuntimeBackend::start`], with an optional SEAT to start on
    /// (issue #552).
    ///
    /// `None` mints a fresh short id and generation 1, which is every
    /// ordinary start. `Some` starts a brand-new conversation that takes over
    /// an existing seat: a rollover successor keeps the seat's stable short
    /// id -- that address is what mail, `zirv ctx nudge` and `zirv ctx status`
    /// resolve, and by design it does not move across a rollover -- and runs
    /// under the generation `seat::commit` promoted, which is what every
    /// fence on its writes compares against. The logical session id is still
    /// fresh: this is a NEW conversation, not a resumed one.
    pub fn start_on_seat(
        &mut self,
        spec: &SessionSpec,
        seat: Option<(&str, u64)>,
    ) -> CtxResult<SessionHandle> {
        super::super::require_native_available()?;
        if spec.runtime != RuntimeKind::Native {
            return Err(RuntimeError::Unsupported(format!(
                "native backend cannot start a `{}` session",
                spec.runtime
            ))
            .into());
        }
        let logical_id = uuid::Uuid::new_v4().to_string();
        let (short, generation) = match seat {
            Some((short, generation)) if !short.is_empty() => {
                (short.to_string(), generation.max(1))
            }
            _ => (logical_id.chars().take(8).collect::<String>(), 1),
        };
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: logical_id.clone(),
            short: short.clone(),
            generation,
            role: spec.role.clone(),
            surface: spec.surface,
            conversation: Some(BackendConversationRef {
                agent: RuntimeKind::Native.as_str().to_string(),
                conversation: logical_id.clone(),
            }),
        };
        let mut record = NativeSessionRecord {
            short,
            generation,
            role: spec.role.clone(),
            surface: spec.surface,
            state: SessionState::Idle,
            cancel: Arc::new(CancellationFlag::default()),
            events: Vec::new(),
            journal_session: None,
        };
        record.push(
            &logical_id,
            super::super::protocol::RuntimeEvent::Started {
                session: handle.clone(),
            },
        );
        self.sessions.insert(logical_id, record);
        Ok(handle)
    }

    /// Registers an EXISTING handle under this backend and binds it to the
    /// journal session its conversation lives in.
    ///
    /// Idempotent, and the only way a session this process did not itself
    /// `start` -- one recovered by [`resume_journal`] after a crash, or one a
    /// persistent runtime is re-attaching to -- becomes drivable. Until a
    /// handle is adopted its inputs are accepted but tracked only in memory,
    /// which is why `run_headless` adopts before it accepts anything.
    pub fn adopt(
        &mut self,
        session: &SessionHandle,
        journal_session: JournalSessionId,
    ) -> CtxResult<()> {
        super::super::require_native_available()?;
        match self.sessions.get_mut(&session.logical_id) {
            Some(entry) => {
                entry.generation = session.generation;
                entry.journal_session = Some(journal_session);
            }
            None => {
                self.sessions.insert(
                    session.logical_id.clone(),
                    NativeSessionRecord {
                        short: session.short.clone(),
                        generation: session.generation,
                        role: session.role.clone(),
                        surface: session.surface,
                        state: SessionState::Idle,
                        cancel: Arc::new(CancellationFlag::default()),
                        events: Vec::new(),
                        journal_session: Some(journal_session),
                    },
                );
            }
        }
        Ok(())
    }

    /// The journal, for a caller that drives the loop itself against the same
    /// database this backend acknowledges input into.
    pub fn journal_mut(&mut self) -> Option<&mut Journal> {
        self.journal.as_mut()
    }

    /// The journal for a read-only caller -- history and cursor pages
    /// (issue #489) need no write access, and asking for `&mut` to run a
    /// `SELECT` would force every reader to take the writer's place in the
    /// queue.
    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    fn mint_message_id(&mut self) -> CtxResult<MessageId> {
        self.minted += 1;
        Ok(MessageId::new(format!(
            "input-{}-{}",
            uuid::Uuid::new_v4().simple(),
            self.minted
        ))?)
    }

    /// Records one accepted input durably, BEFORE the caller is told it was
    /// accepted. A session with no journal bound is a no-op, not an error:
    /// the in-memory protocol surface has no durable store to lose it from.
    fn record_input(
        &mut self,
        session: &SessionHandle,
        input: &str,
        steering: bool,
    ) -> CtxResult<()> {
        let Some(entry) = self.sessions.get(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        let Some(journal_session) = entry.journal_session.clone() else {
            return Ok(());
        };
        let generation = entry.generation;
        if self.journal.is_none() {
            return Ok(());
        }
        let message_id = self.mint_message_id()?;
        let at_ms = now_ms();
        let journal = self.journal.as_mut().expect("checked just above");
        acknowledge_input(
            journal,
            &journal_session,
            generation,
            message_id,
            input,
            steering,
            at_ms,
        )
    }

    /// Persist caller idempotency keys as journal message IDs so retries across restarts deduplicate; unkeyed input remains fresh, and hosted input may queue during a turn. (#489)
    pub fn accept_input(
        &mut self,
        session: &SessionHandle,
        input: &str,
        steering: bool,
        key: Option<&str>,
    ) -> CtxResult<AcceptedInput> {
        let logical_id = session.logical_id.clone();
        // Fences on generation before anything is written.
        self.resolve_current_mut(session)?;
        let Some(entry) = self.sessions.get(&logical_id) else {
            return Err(RuntimeError::UnknownSession(logical_id).into());
        };
        let journal_session = entry.journal_session.clone();
        let generation = entry.generation;
        let message_id = match key {
            Some(key) => idempotent_message_id(key)?,
            None => self.mint_message_id()?,
        };
        let (Some(journal_session), Some(journal)) = (journal_session, self.journal.as_mut())
        else {
            // No durable store bound: the in-memory protocol surface has
            // nothing to deduplicate against, and says so by reporting the id
            // it would have used rather than pretending to a guarantee.
            return Ok(AcceptedInput {
                message_id,
                duplicate: false,
            });
        };
        let at_ms = now_ms();
        match journal.acknowledge_input(
            &journal_session,
            generation,
            &EventScope::default(),
            message_id.clone(),
            input.to_string(),
            steering,
            Some(at_ms),
            at_ms / 1000,
        ) {
            Ok(_) => Ok(AcceptedInput {
                message_id,
                duplicate: false,
            }),
            // The one error that is not a failure: this exact input is already
            // on disk under this exact identity, so the first attempt won and
            // nothing else may happen.
            Err(super::super::journal::JournalError::DuplicateId { .. }) => Ok(AcceptedInput {
                message_id,
                duplicate: true,
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn resolve_current_mut(
        &mut self,
        session: &SessionHandle,
    ) -> CtxResult<&mut NativeSessionRecord> {
        let Some(entry) = self.sessions.get_mut(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        if session.generation < entry.generation {
            return Err(RuntimeError::StaleGeneration {
                expected: entry.generation,
                got: session.generation,
            }
            .into());
        }
        Ok(entry)
    }

    /// The cancellation flag for a live session, so a caller that drives a
    /// [`NativeLoop`] itself shares the one `interrupt` sets.
    pub fn cancellation(&self, session: &SessionHandle) -> Option<Arc<CancellationFlag>> {
        self.sessions
            .get(&session.logical_id)
            .map(|entry| Arc::clone(&entry.cancel))
    }

    /// The session-level state, for a caller that needs to know whether a
    /// turn is in flight before issuing one.
    pub fn state(&self, session: &SessionHandle) -> Option<SessionState> {
        self.sessions
            .get(&session.logical_id)
            .map(|entry| entry.state)
    }
}

impl Default for NativeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeSessionRecord {
    pub(super) fn push(&mut self, logical_id: &str, event: super::super::protocol::RuntimeEvent) {
        let revision = self.events.len() as u64 + 1;
        self.events.push(super::super::protocol::EventEnvelope {
            version: super::super::protocol::PROTOCOL_VERSION,
            revision,
            session: logical_id.to_string(),
            generation: self.generation,
            event,
        });
    }
}

impl RuntimeBackend for NativeBackend {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Native
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steer: true,
            interrupt: true,
            resume: true,
            events: true,
            surfaces: vec![
                UiSurface::Headless,
                UiSurface::Terminal,
                UiSurface::DashboardPane,
            ],
        }
    }

    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle> {
        self.start_on_seat(spec, None)
    }

    /// A submit is only ever accepted by an idle session. Driving the turn is
    /// the caller's own call into [`NativeLoop::run_to_completion`], because
    /// the loop needs a journal and a provider this trait deliberately does
    /// not carry -- see `run_headless`.
    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        {
            let entry = self.resolve_current_mut(session)?;
            if entry.state == SessionState::Running {
                return Err(RuntimeError::Busy(logical_id).into());
            }
        }
        // Durable FIRST, in-memory bookkeeping after: a crash between the two
        // costs a protocol event a subscriber can re-derive, never the input
        // itself. The reverse order would let a caller see `Ok` for an input
        // that exists nowhere but this process's memory.
        self.record_input(session, input, false)?;
        let entry = self.resolve_current_mut(session)?;
        entry.state = SessionState::Running;
        entry.push(
            &logical_id,
            super::super::protocol::RuntimeEvent::TurnStarted,
        );
        entry.push(
            &logical_id,
            super::super::protocol::RuntimeEvent::AssistantText {
                text: format!("queued: {input}"),
            },
        );
        Ok(())
    }

    /// Steering is accepted at ANY time, including mid-turn: that is the
    /// point. It joins the conversation at the next delivery boundary, and is
    /// durable from the moment it is accepted.
    fn steer(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        // Fences on generation before anything is written.
        self.resolve_current_mut(session)?;
        self.record_input(session, input, true)?;
        let entry = self.resolve_current_mut(session)?;
        entry.push(
            &logical_id,
            super::super::protocol::RuntimeEvent::AssistantText {
                text: format!("steering queued: {input}"),
            },
        );
        Ok(())
    }

    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        let entry = self.resolve_current_mut(session)?;
        entry.cancel.cancel();
        entry.state = SessionState::Interrupted;
        entry.push(
            &logical_id,
            super::super::protocol::RuntimeEvent::Interrupted,
        );
        Ok(())
    }

    /// Resume with a new generation; unfinished durable executions become `OutcomeUnknown` and are never silently retried.
    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle> {
        let logical_id = session.logical_id.clone();
        let journal_session = self
            .sessions
            .get(&logical_id)
            .ok_or_else(|| RuntimeError::UnknownSession(logical_id.clone()))?
            .journal_session
            .clone();
        let resumed = match (journal_session.as_ref(), self.journal.as_mut()) {
            (Some(journal_session), Some(journal)) => {
                Some(resume_journal(journal, journal_session, now_ms())?)
            }
            _ => None,
        };

        let Some(entry) = self.sessions.get_mut(&logical_id) else {
            return Err(RuntimeError::UnknownSession(logical_id).into());
        };
        entry.generation = match &resumed {
            Some(resumed) => resumed.generation,
            None => entry.generation + 1,
        };
        // A resume is a fresh cancellation scope: the flag an earlier
        // interrupt set must not silently cancel the resumed turn.
        entry.cancel = Arc::new(CancellationFlag::default());
        entry.state = SessionState::Idle;
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: logical_id.clone(),
            short: entry.short.clone(),
            generation: entry.generation,
            role: entry.role.clone(),
            surface: entry.surface,
            conversation: Some(BackendConversationRef {
                agent: RuntimeKind::Native.as_str().to_string(),
                conversation: logical_id.clone(),
            }),
        };
        entry.push(
            &logical_id,
            super::super::protocol::RuntimeEvent::Started {
                session: handle.clone(),
            },
        );
        // The input a resume carries is acknowledged against the NEW
        // generation, durably, exactly like any other accepted input.
        if let Some(input) = input {
            self.record_input(&handle, input, false)?;
            let entry = self.resolve_current_mut(&handle)?;
            entry.push(
                &logical_id,
                super::super::protocol::RuntimeEvent::AssistantText {
                    text: format!("queued: {input}"),
                },
            );
        }
        Ok(handle)
    }

    fn subscribe(
        &mut self,
        session: &SessionHandle,
        after_revision: u64,
    ) -> CtxResult<Vec<super::super::protocol::EventEnvelope>> {
        let Some(entry) = self.sessions.get(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        Ok(entry
            .events
            .iter()
            .filter(|event| event.revision > after_revision)
            .cloned()
            .collect())
    }
}
#[cfg(test)]
mod tests {
    use super::super::super::super::provider::Protocol;
    use super::super::super::super::provider::adapter::Cancellation;
    use super::super::super::journal::MessageRole;
    use super::super::tests::{journal_for, route_for, spec};
    use super::*;

    #[test]
    fn a_second_submit_while_a_turn_is_running_is_busy_not_a_silent_interleave() {
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.submit(&handle, "first").expect("first submit");
        let error = backend.submit(&handle, "second").expect_err("busy");
        assert!(error.to_string().contains("busy"));
        // Steering is always accepted, including mid-turn.
        backend.steer(&handle, "also do this").expect("steered");
    }

    #[test]
    fn a_resume_clears_the_interrupt_flag_and_bumps_the_generation() {
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.interrupt(&handle).expect("interrupted");
        assert!(backend.cancellation(&handle).unwrap().is_cancelled());
        let resumed = backend.resume(&handle, Some("carry on")).expect("resumed");
        assert_eq!(resumed.generation, handle.generation + 1);
        assert!(!backend.cancellation(&resumed).unwrap().is_cancelled());
        assert_eq!(backend.state(&resumed), Some(SessionState::Idle));
        let stale = backend.submit(&handle, "stale").expect_err("stale");
        assert!(stale.to_string().contains("stale generation"));
    }

    #[test]
    fn the_backend_refuses_to_start_a_harness_session() {
        let mut backend = NativeBackend::new();
        let error = backend
            .start(&spec(RuntimeKind::Harness))
            .expect_err("wrong runtime");
        assert!(error.to_string().contains("harness"));
    }

    // -- review round 2 ----------------------------------------------------

    /// Finding 2, the other half: a resume through the backend seam does the
    /// same durable work, not just an in-memory counter bump.
    #[test]
    fn a_backend_resume_with_a_journal_advances_the_stored_generation() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, journal, session) = journal_for(&route);
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.attach_journal(journal);
        backend.adopt(&handle, session.clone()).expect("adopted");

        let resumed = backend.resume(&handle, None).expect("resumed");
        assert_eq!(resumed.generation, 2);
        assert_eq!(
            backend
                .journal_mut()
                .unwrap()
                .session(&session)
                .unwrap()
                .generation,
            2,
            "the backend's generation must be the journal's, not a private counter"
        );
    }

    /// Finding 3: an input the backend accepted is durable before the caller
    /// is told it was accepted, so a crash straight after `Ok` cannot lose it.
    #[test]
    fn backend_input_is_journalled_before_it_is_reported_as_accepted() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, journal, session) = journal_for(&route);
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.attach_journal(journal);
        backend.adopt(&handle, session.clone()).expect("adopted");

        backend.submit(&handle, "do the thing").expect("submitted");
        backend.steer(&handle, "and this too").expect("steered");
        let resumed = backend
            .resume(&handle, Some("carry on"))
            .expect("resumed with input");

        let state = backend
            .journal_mut()
            .unwrap()
            .replay(&session)
            .expect("replay");
        let inputs: Vec<(String, bool)> = state
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| (message.text.clone().unwrap_or_default(), message.steering))
            .collect();
        assert_eq!(
            inputs,
            vec![
                ("do the thing".to_string(), false),
                ("and this too".to_string(), true),
                ("carry on".to_string(), false),
            ]
        );
        // The resume's own input is acknowledged against the NEW generation.
        assert_eq!(resumed.generation, 2);
    }

    /// A backend with no journal is still a usable in-memory protocol surface
    /// -- accepting input is a no-op there, never an error.
    #[test]
    fn a_backend_without_a_journal_still_accepts_input() {
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend
            .submit(&handle, "in memory only")
            .expect("submitted");
        backend.steer(&handle, "also in memory").expect("steered");
        assert_eq!(backend.state(&handle), Some(SessionState::Running));
    }
}
