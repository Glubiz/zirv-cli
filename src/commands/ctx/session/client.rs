//! The client half of the persistent runtime (issue #352): everything that
//! talks TO a runtime rather than being one.
//!
//! It owns presentation and input state and nothing else. There is no pty
//! here, no child process, no supervisor and no registry write -- a client
//! that crashed mid-frame would cost the operator a repaint, never a session.
//! Every call goes through the published protocol v1 client
//! (`api::client::Client`), including the capability check: a server with no
//! terminals does not advertise `session.attach`, so this module says so in
//! one line instead of discovering it through a failed round trip.
//!
//! The key handling deliberately mirrors the dashboard's: `Ctrl+A` is the
//! prefix, `Ctrl+A d` detaches, `Ctrl+A Ctrl+A` sends a literal `Ctrl+A`, and
//! everything else is the child's. An operator who knows the dashboard knows
//! this surface.

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::json;

use super::super::CtxResult;
use super::super::api::client::Client;
use super::super::api::transport::Endpoint;
use super::super::api::wire::{Capability, Method, ScreenView, SessionFacts, SessionState};

/// How long the input poll waits before the loop goes round to repaint. Short
/// enough that a frame lands within one tick of the pty producing it, long
/// enough that an idle attach costs no measurable cpu.
const POLL: Duration = Duration::from_millis(40);

/// The shortest gap between two screen reads. A repaint is one protocol call
/// and one screen write, so this is the client's whole frame budget.
const FRAME: Duration = Duration::from_millis(60);

/// How often an attached client re-reads the session's own facts, to notice
/// that it ended (or was stopped from elsewhere) rather than sitting in front
/// of a frozen screen.
const LIVENESS_POLL: Duration = Duration::from_secs(1);

/// What one keystroke means to an attached client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachVerdict {
    /// Bytes for the session's terminal.
    ToSession(Vec<u8>),
    /// Leave, without touching the session.
    Detach,
    /// The prefix key just armed.
    Pending,
    /// Swallowed: an armed prefix followed by something with no meaning here.
    Ignore,
}

/// Pure: what one keystroke means, given whether the prefix is armed. Returns
/// the new armed state with the verdict, the same shape (and the same prefix)
/// `dash::filter_key` uses -- one muscle memory for both surfaces.
pub fn filter_attach_key(prefix_armed: bool, key: KeyEvent) -> (bool, AttachVerdict) {
    let is_prefix = match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('\u{01}') => true,
        _ => false,
    };
    if !prefix_armed {
        if is_prefix {
            return (true, AttachVerdict::Pending);
        }
        return (
            false,
            AttachVerdict::ToSession(super::super::dash::encode_key(key)),
        );
    }
    if is_prefix {
        // `Ctrl+A Ctrl+A` is a literal prefix byte for the child, exactly as
        // in the dashboard: a harness that binds `Ctrl+A` itself stays
        // reachable.
        return (false, AttachVerdict::ToSession(vec![0x01]));
    }
    match key.code {
        KeyCode::Char('d') | KeyCode::Char('D') => (false, AttachVerdict::Detach),
        _ => (false, AttachVerdict::Ignore),
    }
}

/// This process's client identity. Stable for the life of the process, so a
/// client that drops its connection and reconnects takes its own place back
/// rather than accumulating ghost attachments (see `host::HostSession::
/// clients`). Never reused across processes: the pid is in it.
pub fn client_id(label: &str) -> String {
    format!("{label}-{}", std::process::id())
}

/// Which session a `[name|id]` argument means.
///
/// Pure over a session list so the ambiguity rules are testable without a
/// runtime: an exact id wins, then a unique short-id prefix, then a unique
/// agent or role name. Ambiguity is an error naming the candidates, never a
/// guess -- typing into the wrong agent is not a recoverable mistake.
pub fn resolve_target<'a>(
    sessions: &'a [SessionFacts],
    target: Option<&str>,
) -> Result<&'a SessionFacts, String> {
    let live: Vec<&SessionFacts> = sessions
        .iter()
        .filter(|facts| facts.state != SessionState::Ended)
        .collect();
    let Some(target) = target.map(str::trim).filter(|t| !t.is_empty()) else {
        return match live.as_slice() {
            [only] => Ok(only),
            [] => Err("no live session on this runtime".to_string()),
            many => Err(format!(
                "name a session: {}",
                many.iter()
                    .map(|facts| facts.short.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    };
    if let Some(exact) = sessions.iter().find(|facts| facts.session_id == target) {
        return Ok(exact);
    }
    let matches: Vec<&SessionFacts> = live
        .iter()
        .copied()
        .filter(|facts| {
            facts.short.starts_with(target)
                || facts.agent.as_deref() == Some(target)
                || facts.role.as_deref() == Some(target)
        })
        .collect();
    match matches.as_slice() {
        [only] => Ok(only),
        [] => Err(format!("no live session matches '{target}'")),
        many => Err(format!(
            "'{target}' matches {} sessions: {}",
            many.len(),
            many.iter()
                .map(|facts| facts.short.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Every session the runtime at `endpoint` knows about.
pub fn snapshot(client: &mut Client) -> CtxResult<Vec<SessionFacts>> {
    let value = client.call(Method::SessionSnapshot, json!({}))?;
    let sessions = value.get("sessions").cloned().unwrap_or_else(|| json!([]));
    Ok(serde_json::from_value(sessions)?)
}

/// The refusal a client prints when it reaches a server that owns no
/// terminals. One wording, so `attach`, `detach` and `stop` cannot describe
/// the same situation three different ways.
pub const NO_TERMINALS: &str = "this endpoint serves the runtime protocol but owns no terminals: \
     start the persistent runtime with `zirv session serve` (it needs \
     `[session] persistent = true`, operator-only)";

/// Whether the negotiated capability set lets this client attach at all. The
/// check is LOCAL -- the point of the handshake is that a feature the server
/// never advertised is disabled here rather than attempted and refused.
pub fn can_attach(client: &Client) -> bool {
    client.negotiated().has(Capability::SessionAttach)
}

/// Connects to the runtime endpoint, refusing early and in one sentence when
/// nothing is listening.
pub fn connect(endpoint: &Endpoint) -> CtxResult<Client> {
    if !super::super::api::transport::probe(endpoint) {
        return Err(format!(
            "no zirv runtime is listening on {}: start one with `zirv session serve`",
            endpoint.display()
        )
        .into());
    }
    Client::connect(endpoint)
}

/// How an attach ended, so the caller can pick an exit code and a message
/// without re-deriving either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    /// The operator pressed `Ctrl+A d`. The session keeps running.
    Detached,
    /// The session ended underneath the client.
    SessionEnded,
}

/// Attaches this terminal to `session_id` and runs the interactive loop until
/// the operator detaches or the session ends.
///
/// The session is never touched on the way out: [`AttachOutcome::Detached`]
/// calls `session.detach`, which by construction moves one entry in the
/// runtime's attachment table and nothing else.
pub fn attach_terminal<W: Write>(
    client: &mut Client,
    session_id: &str,
    client_name: &str,
    controller: bool,
    takeover: bool,
    notes: &mut W,
) -> CtxResult<AttachOutcome> {
    let (cols, rows) = crossterm::terminal::size()
        .unwrap_or((super::host::DEFAULT_COLS, super::host::DEFAULT_ROWS));
    let mode = if controller { "controller" } else { "observer" };
    let attached = client.call(
        Method::SessionAttach,
        json!({
            "session_id": session_id,
            "client_id": client_name,
            "mode": mode,
            "rows": rows,
            "cols": cols,
        }),
    );
    let mut controlling = controller;
    match attached {
        Ok(_) => {}
        Err(error) if controller && takeover => {
            writeln!(notes, "zirv session: taking the controller seat ({error})")?;
            client.call(
                Method::SessionTakeover,
                json!({ "session_id": session_id, "client_id": client_name }),
            )?;
            client.call(
                Method::SessionResize,
                json!({
                    "session_id": session_id,
                    "client_id": client_name,
                    "rows": rows,
                    "cols": cols,
                }),
            )?;
        }
        Err(error) if controller => {
            // Refused, not stolen: somebody is typing into this session. Fall
            // back to watching it, and say how to take the seat on purpose.
            writeln!(
                notes,
                "zirv session: {error}\nattaching read-only; pass --takeover to take the keyboard"
            )?;
            controlling = false;
            client.call(
                Method::SessionAttach,
                json!({
                    "session_id": session_id,
                    "client_id": client_name,
                    "mode": "observer",
                }),
            )?;
        }
        Err(error) => return Err(error),
    }

    let outcome = run_attached(client, session_id, client_name, controlling);
    // Detach on EVERY exit path, including an error one: a client that went
    // away without saying so would leave a ghost in the attachment table
    // until something else noticed.
    let _ = client.call(
        Method::SessionDetach,
        json!({ "session_id": session_id, "client_id": client_name }),
    );
    outcome
}

/// The loop itself, with the terminal in raw mode. Split out so
/// [`attach_terminal`] can guarantee the detach call runs whatever happens in
/// here.
fn run_attached(
    client: &mut Client,
    session_id: &str,
    client_name: &str,
    controlling: bool,
) -> CtxResult<AttachOutcome> {
    let _vt = super::super::term::enable_vt_output().ok();
    crossterm::terminal::enable_raw_mode()
        .map_err(|error| format!("zirv session attach: enable_raw_mode failed: {error}"))?;
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen);

    let result = attached_loop(client, session_id, client_name, controlling, &mut stdout);

    let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

fn attached_loop<W: Write>(
    client: &mut Client,
    session_id: &str,
    client_name: &str,
    controlling: bool,
    out: &mut W,
) -> CtxResult<AttachOutcome> {
    let mut armed = false;
    let mut painted: Option<String> = None;
    let mut last_frame = Instant::now() - FRAME;
    let mut last_liveness = Instant::now();
    loop {
        if event::poll(POLL).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    let (next, verdict) = filter_attach_key(armed, key);
                    armed = next;
                    match verdict {
                        AttachVerdict::Detach => return Ok(AttachOutcome::Detached),
                        AttachVerdict::ToSession(bytes) if controlling && !bytes.is_empty() => {
                            client.call(
                                Method::SessionSendInput,
                                json!({
                                    "session_id": session_id,
                                    "client_id": client_name,
                                    "mode": "raw",
                                    "input": String::from_utf8_lossy(&bytes),
                                }),
                            )?;
                            // Repaint on the next pass, not in some later
                            // frame: the operator has to see their own
                            // keystroke land.
                            last_frame = Instant::now() - FRAME;
                        }
                        AttachVerdict::ToSession(_)
                        | AttachVerdict::Pending
                        | AttachVerdict::Ignore => {}
                    }
                }
                Ok(Event::Resize(cols, rows)) if controlling => {
                    client.call(
                        Method::SessionResize,
                        json!({
                            "session_id": session_id,
                            "client_id": client_name,
                            "rows": rows,
                            "cols": cols,
                        }),
                    )?;
                    painted = None;
                }
                Ok(_) => {}
                Err(error) => return Err(error.into()),
            }
        }

        if last_frame.elapsed() >= FRAME {
            last_frame = Instant::now();
            let screen = read_screen(client, session_id, client_name)?;
            if painted.as_deref() != Some(screen.contents.as_str()) {
                out.write_all(screen.contents.as_bytes())?;
                out.flush()?;
                painted = Some(screen.contents);
            }
        }

        if last_liveness.elapsed() >= LIVENESS_POLL {
            last_liveness = Instant::now();
            if session_ended(client, session_id)? {
                return Ok(AttachOutcome::SessionEnded);
            }
        }
    }
}

fn read_screen(client: &mut Client, session_id: &str, client_name: &str) -> CtxResult<ScreenView> {
    let value = client.call(
        Method::SessionScreen,
        json!({ "session_id": session_id, "client_id": client_name }),
    )?;
    Ok(serde_json::from_value(
        value.get("screen").cloned().unwrap_or_default(),
    )?)
}

fn session_ended(client: &mut Client, session_id: &str) -> CtxResult<bool> {
    let value = client.call(Method::SessionGet, json!({ "session_id": session_id }))?;
    let facts: SessionFacts = serde_json::from_value(
        value
            .get("session")
            .cloned()
            .ok_or("the runtime returned no session facts")?,
    )?;
    Ok(facts.state == SessionState::Ended)
}

#[cfg(test)]
mod tests {
    use super::super::super::api::wire::{SessionFacts, SessionState};
    use super::*;

    fn facts(id: &str, short: &str, agent: &str, state: SessionState) -> SessionFacts {
        let mut facts = SessionFacts::new(id);
        facts.short = short.to_string();
        facts.agent = Some(agent.to_string());
        facts.state = state;
        facts
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// The prefix behaves exactly as the dashboard's does, including the
    /// literal-prefix escape: an operator must be able to send `Ctrl+A` to a
    /// harness that binds it.
    #[test]
    fn ctrl_a_d_detaches_and_ctrl_a_ctrl_a_sends_a_literal_prefix() {
        let (armed, verdict) =
            filter_attach_key(false, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(armed);
        assert_eq!(verdict, AttachVerdict::Pending);

        let (armed, verdict) = filter_attach_key(true, key(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(!armed);
        assert_eq!(verdict, AttachVerdict::Detach);

        let (_, verdict) = filter_attach_key(true, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(verdict, AttachVerdict::ToSession(vec![0x01]));
    }

    /// An ordinary keystroke is the session's, not the client's: the attach
    /// surface intercepts the prefix and nothing else.
    #[test]
    fn an_ordinary_keystroke_goes_to_the_session() {
        let (armed, verdict) =
            filter_attach_key(false, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!armed);
        assert_eq!(verdict, AttachVerdict::ToSession(b"x".to_vec()));
        // `d` on its own is the child's too -- only the armed prefix makes it
        // a detach.
        let (_, verdict) = filter_attach_key(false, key(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(verdict, AttachVerdict::ToSession(b"d".to_vec()));
    }

    #[test]
    fn a_bare_attach_picks_the_only_live_session() {
        let sessions = vec![
            facts("id-1", "aaaa1111", "claude", SessionState::Idle),
            facts("id-2", "bbbb2222", "codex", SessionState::Ended),
        ];
        assert_eq!(
            resolve_target(&sessions, None)
                .expect("one live")
                .session_id,
            "id-1"
        );
    }

    /// Ambiguity is refused, and the refusal names the candidates: a guess
    /// here types into the wrong agent.
    #[test]
    fn an_ambiguous_target_is_refused_by_name() {
        let sessions = vec![
            facts("id-1", "aaaa1111", "claude", SessionState::Idle),
            facts("id-2", "aaaa2222", "claude", SessionState::Idle),
        ];
        let error = resolve_target(&sessions, Some("claude")).expect_err("ambiguous");
        assert!(error.contains("aaaa1111"), "{error}");
        assert!(error.contains("aaaa2222"), "{error}");
        let error = resolve_target(&sessions, None).expect_err("ambiguous");
        assert!(error.contains("name a session"), "{error}");
    }

    #[test]
    fn a_short_prefix_and_an_exact_id_both_resolve() {
        let sessions = vec![
            facts("id-1", "aaaa1111", "claude", SessionState::Idle),
            facts("id-2", "bbbb2222", "codex", SessionState::Idle),
        ];
        assert_eq!(
            resolve_target(&sessions, Some("bbbb"))
                .expect("prefix")
                .session_id,
            "id-2"
        );
        assert_eq!(
            resolve_target(&sessions, Some("id-1"))
                .expect("exact")
                .session_id,
            "id-1"
        );
        assert!(resolve_target(&sessions, Some("zzzz")).is_err());
    }

    /// An ENDED session still resolves by its exact id -- `zirv session stop`
    /// on something that already stopped must say "already stopped", not "no
    /// such session" -- but never by a prefix, where it would shadow a live
    /// one.
    #[test]
    fn an_ended_session_resolves_only_by_its_exact_id() {
        let sessions = vec![facts("id-1", "aaaa1111", "claude", SessionState::Ended)];
        assert!(resolve_target(&sessions, Some("id-1")).is_ok());
        assert!(resolve_target(&sessions, Some("aaaa")).is_err());
    }
}
