//! Shared runtime contracts (issue #470): the seam between zirv's own
//! supervision code and whichever backend actually drives an agent
//! conversation. Today the only real backend is [`harness::HarnessBackend`],
//! a facade over the EXISTING harness-process supervision code
//! (`adapters::AgentAdapter`, `supervise::spawn_tapped`) -- this issue adds
//! no new spawn path, only the contract every future backend (issue #469's
//! native runtime, steps N02-N09) will also implement.
//!
//! [`RuntimeKind`] (which backend), `role`/`provider_route`/`model`
//! (`SessionSpec`'s own fields), and [`UiSurface`] (which surface is
//! attached) are deliberately kept as separate fields throughout this
//! module rather than folded into one label: a session's backend, the seat
//! role it was spawned under, the account route it spends, the model it
//! runs, and which UI is currently looking at it are five independent axes
//! that a future caller needs to reason about independently (e.g. attaching
//! a dashboard pane to a session that keeps running headless underneath it
//! changes only `surface`).
//!
//! `protocol.rs` is the versioned wire shape over this trait; `fake.rs` is a
//! deterministic in-memory backend later roadmap steps and built-in checks
//! can run against without spawning anything real; `harness.rs` is the one
//! production backend this issue ships.
//!
//! Nothing in the binary calls into this module outside its own tests yet:
//! wiring a live caller through it (`zirv ctx exec`/`dash`, and the native
//! backend itself) is later roadmap work (issue #469, steps N02-N09).
//! `#![allow(dead_code)]` covers it until then, the same reasoning
//! `dash::pane`'s and `transcript_source`'s own module doc comments already
//! document: a real, fully-tested API with no in-tree caller yet is not the
//! same thing as code that should be deleted.
#![allow(dead_code)]

pub mod capabilities;
pub mod checkpoint;
pub mod compaction;
pub mod context;
pub mod enforcement;
pub mod execution;
pub mod fake;
pub mod fixture;
pub mod harness;
pub mod journal;
pub mod mcp;
pub mod native;
pub mod protocol;
#[cfg(test)]
pub mod testsupport;
pub mod tools;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use self::protocol::EventEnvelope;
use super::CtxResult;
use super::adapters::AgentAdapter;
use super::provider::RouteId;

/// Which backend drives a session's own conversation. `Unknown` is the
/// forward-compat fallback for a value a future build wrote that this one
/// has never heard of -- never a guess at `Harness`, which would silently
/// let a native-only session be treated as one a harness process can be
/// spawned/resumed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Harness,
    Native,
    #[serde(other)]
    Unknown,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::Harness => "harness",
            RuntimeKind::Native => "native",
            RuntimeKind::Unknown => "unknown",
        }
    }
}

/// The one place a `--runtime` FLAG is turned into a decision.
///
/// [`RuntimeKind::from_str`] is infallible on purpose -- it exists to decode
/// a persisted value a future build may have written, where `Unknown` is the
/// only safe answer. A command-line flag is the opposite case: an unrecognised
/// value is an operator mistake and must be a hard error, never a silent fall
/// back to a harness the operator did not ask for.
pub fn selected(flag: &str) -> crate::commands::ctx::CtxResult<RuntimeKind> {
    match flag.parse::<RuntimeKind>() {
        Ok(kind @ (RuntimeKind::Harness | RuntimeKind::Native)) => Ok(kind),
        _ => Err(format!("--runtime '{flag}': expected `harness` or `native`").into()),
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RuntimeKind {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "harness" => RuntimeKind::Harness,
            "native" => RuntimeKind::Native,
            _ => RuntimeKind::Unknown,
        })
    }
}

/// The `--runtime` value meaning "whatever `[runtime]` in `~/.zirv/ctx.toml`
/// says, harness when it says nothing" (issue #491, roadmap N22). It is the
/// clap default for `zirv ctx exec`/`zirv ctx agent`, and `zirv chat` with no
/// `--runtime` resolves the same way, so the operator's opt-in default
/// reaches every entry point without any of them guessing.
pub const CONFIGURED: &str = "configured";

/// Which authority decided a session's backend. Carried so the decision can
/// be *shown* rather than inferred: "native because you asked" and "native
/// because your config says so" are the same outcome and very different
/// facts when a bill arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    /// An explicit `--runtime harness|native` on this invocation.
    Flag,
    /// `[runtime.roles]` named this role.
    RoleTable,
    /// `[runtime] default`.
    ConfiguredDefault,
    /// Nothing said anything: the pre-N22 behaviour, the harness.
    BuiltIn,
}

impl RuntimeSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeSource::Flag => "flag",
            RuntimeSource::RoleTable => "runtime.roles",
            RuntimeSource::ConfiguredDefault => "runtime.default",
            RuntimeSource::BuiltIn => "built-in",
        }
    }
}

/// A resolved backend decision plus the authority behind it, and a note when
/// something configured had to be ignored to get here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeChoice {
    pub kind: RuntimeKind,
    pub source: RuntimeSource,
    /// One line naming a configured value this build does not recognise, and
    /// what was used instead. `None` on every ordinary path.
    pub note: Option<String>,
}

/// The one place [`CONFIGURED`] is turned into a backend.
///
/// Pure: no fs, clock, env or net -- the caller supplies the already-loaded
/// table. An explicit flag always wins; then `[runtime.roles]` for this role;
/// then `[runtime] default`; then the harness. A configured value this build
/// has never heard of degrades to the harness with a note rather than an
/// error, because a typo in a machine-wide config file must not wedge every
/// command on that machine -- `zirv ctx doctor` is where it is reported.
///
/// An unrecognised *flag* stays a hard error ([`selected`]): an operator
/// typing `--runtime natve` at the prompt is asking for one specific thing
/// and must not silently get another.
pub fn resolve(
    flag: &str,
    cfg: &super::config::RuntimeConfig,
    role: &str,
) -> crate::commands::ctx::CtxResult<RuntimeChoice> {
    if !flag.eq_ignore_ascii_case(CONFIGURED) {
        return Ok(RuntimeChoice {
            kind: selected(flag)?,
            source: RuntimeSource::Flag,
            note: None,
        });
    }
    let configured = cfg
        .roles
        .get(role)
        .map(|value| {
            (
                value,
                RuntimeSource::RoleTable,
                format!("runtime.roles.{role}"),
            )
        })
        .or_else(|| {
            cfg.default.as_ref().map(|value| {
                (
                    value,
                    RuntimeSource::ConfiguredDefault,
                    "runtime.default".to_string(),
                )
            })
        });
    let Some((value, source, key)) = configured else {
        return Ok(RuntimeChoice {
            kind: RuntimeKind::Harness,
            source: RuntimeSource::BuiltIn,
            note: None,
        });
    };
    match value.parse::<RuntimeKind>() {
        Ok(kind @ (RuntimeKind::Harness | RuntimeKind::Native)) => Ok(RuntimeChoice {
            kind,
            source,
            note: None,
        }),
        _ => Ok(RuntimeChoice {
            kind: RuntimeKind::Harness,
            source: RuntimeSource::BuiltIn,
            note: Some(format!(
                "{key} = '{value}' is not `harness` or `native`; running on the harness"
            )),
        }),
    }
}

/// [`resolve`]'s impure caller: loads the operator's `[runtime]` table only
/// when there is a [`CONFIGURED`] flag to resolve, so an explicit
/// `--runtime harness|native` still costs no config read at all.
pub fn resolve_for_cli(
    flag: &str,
    repo: &std::path::Path,
    env: super::config::EnvLookup<'_>,
    role: &str,
) -> crate::commands::ctx::CtxResult<RuntimeChoice> {
    if !flag.eq_ignore_ascii_case(CONFIGURED) {
        return resolve(flag, &super::config::RuntimeConfig::default(), role);
    }
    let cfg = super::config::CtxConfig::load(repo, env)?;
    resolve(flag, &cfg.runtime, role)
}

/// Which UI is currently attached to a session, independent of which
/// backend runs it -- a headless launch, an interactive terminal, or a
/// dashboard pane can all sit in front of either an `Harness` or `Native`
/// session. `Unknown` is the same forward-compat fallback `RuntimeKind`
/// documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiSurface {
    #[default]
    Headless,
    Terminal,
    DashboardPane,
    #[serde(other)]
    Unknown,
}

impl UiSurface {
    pub fn as_str(&self) -> &'static str {
        match self {
            UiSurface::Headless => "headless",
            UiSurface::Terminal => "terminal",
            UiSurface::DashboardPane => "dashboardpane",
            UiSurface::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for UiSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for UiSurface {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "headless" => UiSurface::Headless,
            "terminal" => UiSurface::Terminal,
            "dashboardpane" => UiSurface::DashboardPane,
            _ => UiSurface::Unknown,
        })
    }
}

/// Everything a [`RuntimeBackend::start`] needs to launch one session.
/// Backend, seat role, provider route, model and UI surface are separate
/// fields on purpose -- see this module's own doc comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    #[serde(default)]
    pub runtime: RuntimeKind,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub provider_route: Option<RouteId>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub surface: UiSurface,
    pub cwd: PathBuf,
    pub prompt: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// An opaque, per-backend continuation reference -- e.g. the harness's own
/// conversation id (`sessions::native_conversation`'s value) for
/// [`harness::HarnessBackend`]. Never interpreted outside the backend that
/// produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConversationRef {
    pub agent: String,
    pub conversation: String,
}

/// A live session, as a caller needs to keep referring to it across calls.
/// `logical_id` is the zirv session uuid (`event::SessionId`'s own string
/// form); `short` is the existing short id; `generation` is the seat
/// generation (`seat::Seat::generation`) this handle was minted at -- a
/// `resume` bumps it, and a command issued against a handle whose
/// generation has since gone stale must be refused rather than silently
/// answered by the new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub runtime: RuntimeKind,
    pub logical_id: String,
    pub short: String,
    pub generation: u64,
    pub role: String,
    pub surface: UiSurface,
    #[serde(default)]
    pub conversation: Option<BackendConversationRef>,
}

impl SessionHandle {
    /// Changes ONLY `surface` -- attaching a different UI to an already-live
    /// session must never touch its identity, generation, role or backend
    /// conversation reference.
    pub fn attached(mut self, surface: UiSurface) -> Self {
        self.surface = surface;
        self
    }
}

/// Which rot/dashboard-relevant operations a backend actually supports, so a
/// caller can degrade gracefully instead of calling and catching
/// `RuntimeError::Unsupported`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    pub steer: bool,
    pub interrupt: bool,
    pub resume: bool,
    pub events: bool,
    #[serde(default)]
    pub surfaces: Vec<UiSurface>,
}

/// The seam every conversation-driving backend implements: today only
/// [`harness::HarnessBackend`] (a facade over the existing harness-process
/// code) and, for tests and later roadmap steps, [`fake::FakeNativeBackend`].
pub trait RuntimeBackend: std::fmt::Debug {
    fn kind(&self) -> RuntimeKind;
    fn capabilities(&self) -> RuntimeCapabilities;
    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle>;
    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()>;
    fn steer(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()>;
    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()>;
    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle>;
    fn subscribe(
        &mut self,
        session: &SessionHandle,
        after_revision: u64,
    ) -> CtxResult<Vec<EventEnvelope>>;
}

/// The runtime-contract-specific failures a [`RuntimeBackend`] reports, on
/// top of whatever a concrete backend's own I/O already returns as a plain
/// boxed error. [`protocol::dispatch`] downcasts for exactly these four to
/// pick a structured [`protocol::ErrorCode`]; anything else maps to
/// `ErrorCode::Backend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// This backend/operation combination is not implemented -- e.g.
    /// `HarnessBackend::steer`, or `select(RuntimeKind::Native, ..)` before
    /// issue #469's later roadmap steps land.
    Unsupported(String),
    /// `session` names no session this backend instance currently tracks.
    UnknownSession(String),
    /// A turn is already in flight for this session; the caller must wait
    /// for it to finish (or `interrupt`) before submitting another.
    Busy(String),
    /// `session`'s generation no longer matches the backend's own record for
    /// it -- a command issued against a handle a `resume` has since
    /// superseded.
    StaleGeneration { expected: u64, got: u64 },
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Unsupported(what) => write!(f, "unsupported: {what}"),
            RuntimeError::UnknownSession(id) => write!(f, "unknown session: {id}"),
            RuntimeError::Busy(id) => write!(f, "session busy: {id}"),
            RuntimeError::StaleGeneration { expected, got } => {
                write!(f, "stale generation: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Picks the backend for `kind`. `Harness` needs a real adapter (this is
/// where the existing harness code is actually reached); `Native` is
/// [`native::NativeBackend`], the real agent loop issue #478 (N09) shipped,
/// and needs no adapter at all -- a native session never spawns a coding
/// harness. `Unknown` still fails closed: guessing `Harness` would let a
/// native-only session be treated as one a harness process can be spawned for.
///
/// [`fake::FakeNativeBackend`] is deliberately not reachable through this
/// function: it exists for tests and for roadmap steps that want a
/// deterministic stand-in, and both select it directly as a
/// `Box<dyn RuntimeBackend>`.
pub fn select(
    kind: RuntimeKind,
    adapter: Option<Box<dyn AgentAdapter>>,
) -> CtxResult<Box<dyn RuntimeBackend>> {
    match kind {
        RuntimeKind::Harness => {
            let adapter = adapter.ok_or_else(|| {
                Box::new(RuntimeError::Unsupported(
                    "harness runtime requires an adapter".to_string(),
                )) as Box<dyn std::error::Error>
            })?;
            Ok(Box::new(harness::HarnessBackend::new(adapter)))
        }
        RuntimeKind::Native => Ok(Box::new(native::NativeBackend::new())),
        RuntimeKind::Unknown => {
            Err(RuntimeError::Unsupported("unknown runtime kind".to_string()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_kind_round_trips_through_display_and_from_str() {
        for kind in [RuntimeKind::Harness, RuntimeKind::Native] {
            let parsed: RuntimeKind = kind.to_string().parse().expect("infallible");
            assert_eq!(parsed, kind);
        }
        assert_eq!("garbage".parse::<RuntimeKind>(), Ok(RuntimeKind::Unknown));
    }

    #[test]
    fn ui_surface_round_trips_through_display_and_from_str() {
        for surface in [
            UiSurface::Headless,
            UiSurface::Terminal,
            UiSurface::DashboardPane,
        ] {
            let parsed: UiSurface = surface.to_string().parse().expect("infallible");
            assert_eq!(parsed, surface);
        }
        assert_eq!("garbage".parse::<UiSurface>(), Ok(UiSurface::Unknown));
    }

    #[test]
    fn attached_changes_only_the_surface() {
        let handle = SessionHandle {
            runtime: RuntimeKind::Harness,
            logical_id: "session-1".to_string(),
            short: "abcd1234".to_string(),
            generation: 3,
            role: "orchestrator".to_string(),
            surface: UiSurface::Headless,
            conversation: Some(BackendConversationRef {
                agent: "claude".to_string(),
                conversation: "conv-1".to_string(),
            }),
        };
        let attached = handle.clone().attached(UiSurface::DashboardPane);
        assert_eq!(attached.surface, UiSurface::DashboardPane);
        assert_eq!(attached.logical_id, handle.logical_id);
        assert_eq!(attached.short, handle.short);
        assert_eq!(attached.generation, handle.generation);
        assert_eq!(attached.role, handle.role);
        assert_eq!(attached.conversation, handle.conversation);
    }

    #[test]
    fn select_harness_without_an_adapter_is_a_clear_error() {
        let error = select(RuntimeKind::Harness, None).expect_err("no adapter");
        assert!(error.to_string().contains("requires an adapter"));
    }

    #[test]
    fn select_native_needs_no_adapter_at_all() {
        // Acceptance criterion (f) of issue #478 at the seam: selecting the
        // native backend must not require -- or probe for -- an installed
        // coding harness.
        let backend = select(RuntimeKind::Native, None).expect("native backend");
        assert_eq!(backend.kind(), RuntimeKind::Native);
        assert!(backend.capabilities().steer);
        assert!(backend.capabilities().interrupt);
    }

    #[test]
    fn select_unknown_is_an_error() {
        assert!(select(RuntimeKind::Unknown, None).is_err());
    }

    fn runtime_config(toml: &str) -> super::super::config::RuntimeConfig {
        toml::from_str(toml).expect("runtime table")
    }

    /// Issue #491: the opt-in ladder, in the one order that keeps an explicit
    /// request sovereign -- flag, then the role table, then the default, then
    /// the pre-N22 harness.
    #[test]
    fn an_explicit_flag_outranks_every_configured_native_default() {
        let cfg = runtime_config("default = 'native'\n[roles]\nworker = 'native'\n");
        let choice = resolve("harness", &cfg, "worker").expect("resolve");
        assert_eq!(choice.kind, RuntimeKind::Harness);
        assert_eq!(choice.source, RuntimeSource::Flag);
    }

    #[test]
    fn a_role_entry_outranks_the_configured_default() {
        let cfg = runtime_config("default = 'native'\n[roles]\nworker = 'harness'\n");
        let worker = resolve(CONFIGURED, &cfg, "worker").expect("resolve");
        assert_eq!(worker.kind, RuntimeKind::Harness);
        assert_eq!(worker.source, RuntimeSource::RoleTable);
        let reviewer = resolve(CONFIGURED, &cfg, "reviewer").expect("resolve");
        assert_eq!(reviewer.kind, RuntimeKind::Native);
        assert_eq!(reviewer.source, RuntimeSource::ConfiguredDefault);
    }

    /// The compatibility promise N22 ships on: an operator config written
    /// before this key existed resolves exactly the way every build before
    /// N22 behaved, and says so.
    #[test]
    fn an_unconfigured_runtime_table_still_resolves_to_the_harness() {
        let choice = resolve(CONFIGURED, &runtime_config(""), "orchestrator").expect("resolve");
        assert_eq!(choice.kind, RuntimeKind::Harness);
        assert_eq!(choice.source, RuntimeSource::BuiltIn);
        assert_eq!(choice.note, None);
    }

    /// A typo in a machine-wide config file must not wedge every command on
    /// that machine, but it must not be silent either.
    #[test]
    fn an_unrecognised_configured_value_degrades_to_the_harness_with_a_note() {
        let cfg = runtime_config("default = 'natve'\n");
        let choice = resolve(CONFIGURED, &cfg, "worker").expect("resolve");
        assert_eq!(choice.kind, RuntimeKind::Harness);
        assert_eq!(choice.source, RuntimeSource::BuiltIn);
        assert!(
            choice
                .note
                .as_deref()
                .is_some_and(|note| note.contains("runtime.default") && note.contains("natve")),
            "got {:?}",
            choice.note
        );
        // A typo in the FLAG stays a hard error: that operator is at a prompt
        // asking for one specific thing.
        assert!(resolve("natve", &cfg, "worker").is_err());
    }

    /// Issue #492 (roadmap N23) item 3 and its "mixed-runtime operation and
    /// return to legacy defaults remain usable" criterion, end to end on one
    /// board: a wrapped seat and a native seat sit side by side, exchange
    /// directed mail in both directions, and then the operator takes the
    /// configured default back to the harness.
    ///
    /// The return is the half that is easy to get wrong, so it is what the
    /// assertions are about: flipping the default must change only what an
    /// UNFLAGGED session resolves to. Both seats keep their own runtime,
    /// each conversation reference still resolves under the runtime that
    /// recorded it and under no other, and the mail each seat had not read
    /// yet is still there to read exactly once. Nothing about a return to
    /// legacy defaults may cost an operator state they already had.
    #[test]
    fn a_mixed_board_exchanges_mail_and_survives_a_return_to_the_harness_default() {
        use super::super::config::CtxConfig;
        use super::super::state::StateDir;
        use super::super::{mail, seat, sessions};

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = "-work-repo";

        let seats = [
            (
                "11111111-1111-4000-8000-000000000000",
                "native",
                RuntimeKind::Native,
            ),
            (
                "22222222-2222-4000-8000-000000000000",
                "claude",
                RuntimeKind::Harness,
            ),
        ];
        let mut shorts = Vec::new();
        for (session, agent, runtime) in seats {
            let short = sessions::short_id(session);
            seat::register(
                &state,
                &short,
                session,
                agent,
                Some("standard"),
                "anthropic",
                "worker",
                false,
                400,
            )
            .expect("register");
            let mut record = seat::load(&state, &short).expect("seat");
            record.runtime = runtime;
            seat::store(&state, &record).expect("store");
            sessions::record_conversation_on(
                &state,
                &short,
                agent,
                session,
                &format!("{agent}-conversation"),
                runtime,
            );
            shorts.push(short);
        }

        // One directed message each way, across the runtime boundary.
        for (from, to) in [(0usize, 1usize), (1, 0)] {
            let msg = mail::Message {
                from_session: shorts[from].clone(),
                from_agent: seats[from].1.to_string(),
                to: "any".to_string(),
                to_session: Some(shorts[to].clone()),
                sent: 1_700_000_000 + from as u64,
                body: format!("from {} to {}", seats[from].1, seats[to].1),
            };
            mail::store(&state, slug, &msg, &cfg).expect("store mail");
        }
        for (index, short) in shorts.iter().enumerate() {
            let waiting = mail::list(&state, slug, None, Some(short)).expect("list");
            assert_eq!(
                waiting.len(),
                1,
                "each seat sees exactly the message addressed to it: {index} {waiting:?}"
            );
        }

        // The operator takes the default back to the harness.
        let returned = runtime_config("");
        for role in ["worker", "reviewer", "orchestrator"] {
            let choice = resolve(CONFIGURED, &returned, role).expect("resolve");
            assert_eq!(
                choice.kind,
                RuntimeKind::Harness,
                "an unflagged {role} session is back on the harness"
            );
        }

        // ...and every piece of state both seats already had is untouched.
        for (index, (session, agent, runtime)) in seats.into_iter().enumerate() {
            let short = &shorts[index];
            let record = seat::load(&state, short).expect("seat survives the return");
            assert_eq!(record.runtime, runtime, "{agent} keeps its own runtime");
            assert_eq!(
                sessions::native_conversation(&state, short, agent, session, runtime).as_deref(),
                Some(format!("{agent}-conversation").as_str()),
                "{agent} still resolves its own conversation"
            );
            let other = match runtime {
                RuntimeKind::Native => RuntimeKind::Harness,
                _ => RuntimeKind::Native,
            };
            assert_eq!(
                sessions::native_conversation(&state, short, agent, session, other),
                None,
                "{agent}'s conversation is never handed to the other runtime"
            );
            let waiting = mail::list(&state, slug, None, Some(short)).expect("list after");
            assert_eq!(
                waiting.len(),
                1,
                "{agent}'s unread mail survives the return"
            );
            mail::consume_and_log(&state, slug, &waiting[0].0, short, "exec", "exec:test")
                .expect("consume once");
            assert!(
                mail::list(&state, slug, None, Some(short))
                    .expect("list again")
                    .is_empty(),
                "{agent} is not handed the same message twice"
            );
        }
    }
}
