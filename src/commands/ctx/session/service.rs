//! The runtime service process (issue #352): what `zirv session serve` runs.
//!
//! It owns three things and nothing else:
//!
//! - a [`RuntimeHost`] (the pty/ConPTY sessions, their supervisors and their
//!   registry records),
//! - an [`ApiServer`] with that host attached, bound to the ordinary protocol
//!   v1 endpoint, and
//! - the durable [`namespace`] record and topology that let a later process --
//!   a client, or the service's own successor -- tell whether this runtime is
//!   still there and what it was carrying.
//!
//! Two rules are worth stating where they are implemented:
//!
//! - **The service never claims a live namespace.** [`claim_for`] answers off
//!   process START IDENTITY (see `namespace::classify`), so a recycled pid is
//!   replaceable and a live runtime is not, on both platforms.
//! - **Shutdown is not a kill.** [`RuntimeService::shutdown`] drains the
//!   topology and removes the record; it puts sessions through the existing
//!   child-termination ladder ONLY when the operator asked for that
//!   (`--stop-sessions`, or `zirv session stop <id>` one at a time).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::CtxResult;
use super::super::api::server::{
    ApiServer, NativeHost, RegistrySource, RunningServer, SessionHost, SessionSource,
};
use super::super::api::transport::Endpoint;
use super::super::api::wire::SessionFacts;
use super::super::config::CtxConfig;
use super::super::state::{self, StateDir};
use super::host::{RuntimeHost, SpawnSpec, TopologyEntry};
use super::namespace::{self, Liveness, Namespace, ProcessIdentity};
use super::native::{NativeEnvironment, NativeRestoreReport, NativeSessions, ProviderEnvironment};

/// How often the service drains every session's pty into its parser. Fast
/// enough that a reattaching client sees a current screen, slow enough that an
/// idle runtime costs nothing measurable -- the same order as the dashboard's
/// own tick.
pub const PUMP_INTERVAL: Duration = Duration::from_millis(25);

/// How often the namespace record's `last_client_at` is refreshed. Only ever a
/// SECONDARY staleness signal (start identity is the primary one), so it is
/// deliberately coarse.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// What a new service may do with the namespace record it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Nothing is there.
    Free,
    /// Something is there but its publisher is gone or its pid was recycled.
    Replace(Liveness),
    /// A live runtime already owns this namespace.
    Refuse(Liveness),
}

/// Whether this process may claim `record`'s namespace. Pure over the probe so
/// both answers -- including the recycled-pid one no test can arrange for real
/// -- are provable on either platform.
pub fn claim_for(
    record: Option<&Namespace>,
    probe: &dyn Fn(u32) -> ProcessIdentity,
    now: u64,
    stale_after_secs: u64,
) -> Claim {
    let Some(record) = record else {
        return Claim::Free;
    };
    let liveness = namespace::classify(record, probe, now, stale_after_secs);
    if liveness.is_replaceable() {
        Claim::Replace(liveness)
    } else {
        Claim::Refuse(liveness)
    }
}

/// `<state>/runtime/<name>.shutdown` -- the file `zirv session stop --runtime`
/// drops and the serve loop notices.
///
/// A file rather than a protocol method on purpose: protocol v1 is frozen (see
/// `api::wire::METHODS`), and inventing a private `server.shutdown` frame for
/// one CLI verb is exactly the "split the daemon through private messages"
/// shape issue #352 rules out. The file is written into the same owner-only
/// state directory the endpoint itself lives in, so whoever can request a
/// shutdown could already connect and stop every session individually.
pub fn shutdown_path(state: &StateDir, name: &str) -> PathBuf {
    namespace::runtime_dir(state).join(format!("{}.shutdown", state::provider_slug(name)))
}

pub fn request_shutdown(state: &StateDir, name: &str) -> CtxResult<()> {
    state::create_private_dir_all(&namespace::runtime_dir(state))?;
    state::write_private(&shutdown_path(state, name), "stop\n")?;
    Ok(())
}

pub fn shutdown_requested(state: &StateDir, name: &str) -> bool {
    shutdown_path(state, name).exists()
}

pub fn clear_shutdown(state: &StateDir, name: &str) {
    let _ = std::fs::remove_file(shutdown_path(state, name));
}

/// The session list this server publishes: the runtime's own terminals first,
/// then every OTHER registry record, so `zirv ctx status` and a protocol
/// client see one world rather than two.
///
/// Host facts win on a collision, and a collision is the normal case: a
/// host-owned session registers an ordinary registry record (that is what
/// keeps pacing, budgets, rot and mail working for it), so the same session
/// legitimately arrives from both sources. The host's own view is the more
/// specific one -- it knows whether a client is attached.
#[derive(Debug)]
pub struct HostSource {
    host: Arc<RuntimeHost>,
    /// Issue #489: the native conversations this runtime owns, published from
    /// the same source as its terminals so a client sees ONE session list
    /// rather than one per backend.
    native: Arc<NativeSessions>,
    registry: RegistrySource,
}

impl HostSource {
    pub fn new(host: Arc<RuntimeHost>, native: Arc<NativeSessions>, state: StateDir) -> Self {
        Self {
            host,
            native,
            registry: RegistrySource::new(state),
        }
    }
}

impl SessionSource for HostSource {
    fn sessions(&self) -> Vec<SessionFacts> {
        let mut facts = self.host.sessions();
        facts.extend(NativeHost::sessions(self.native.as_ref()));
        let owned: BTreeSet<String> = facts.iter().map(|entry| entry.session_id.clone()).collect();
        facts.extend(
            self.registry
                .sessions()
                .into_iter()
                .filter(|entry| !owned.contains(&entry.session_id)),
        );
        facts
    }
}

/// What a tier-2 restore actually did, kept apart from the doing so the
/// honesty rule is reportable: `resumed` are sessions with a verified
/// conversation reference, `skipped` are entries whose processes are simply
/// gone and are never described as anything else.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub resumed: Vec<String>,
    pub skipped: Vec<TopologyEntry>,
    pub failed: Vec<(String, String)>,
}

/// A bound, serving runtime.
#[derive(Debug)]
pub struct RuntimeService {
    state: StateDir,
    namespace: String,
    instance: String,
    host: Arc<RuntimeHost>,
    /// Issue #489: the native conversations. A second host on ONE server and
    /// one endpoint -- not a second service, and not a private wire.
    native: Arc<NativeSessions>,
    /// The operator configuration this service started under, held so the
    /// mail sweep does not reload it from disk on every heartbeat.
    cfg: CtxConfig,
    /// Per-session mail advisories already delivered, so an orchestrator seat
    /// is told about one message once rather than on every heartbeat -- the
    /// same map, and the same purpose, `dash::mail_sweep` keeps for a pane.
    advised: std::sync::Mutex<std::collections::HashMap<String, super::super::mail::AdvisedIds>>,
    /// Dropped last: dropping it stops the accept loop and removes the
    /// endpoint.
    running: RunningServer,
}

impl RuntimeService {
    /// Binds the endpoint and publishes the namespace record. Fails rather
    /// than steals when a live runtime already owns the namespace.
    pub fn start(state: StateDir, namespace_name: &str, cfg: &CtxConfig) -> CtxResult<Self> {
        Self::start_with(
            state,
            namespace_name,
            cfg,
            Arc::new(ProviderEnvironment::new(
                Default::default(),
                cfg.supervise.max_writers,
            )),
        )
    }

    /// The form a test uses: the same service, with the native environment
    /// (route resolution and turn execution) injected, so every durable and
    /// protocol behaviour below can be proven without a provider, a credential
    /// or a network.
    pub fn start_with(
        state: StateDir,
        namespace_name: &str,
        cfg: &CtxConfig,
        environment: Arc<dyn NativeEnvironment>,
    ) -> CtxResult<Self> {
        let now = state::now_secs();
        let stale_after = cfg.session.stale_after_secs_or_default();
        let existing = namespace::read(&state, namespace_name);
        match claim_for(
            existing.as_ref(),
            &namespace::process_identity,
            now,
            stale_after,
        ) {
            Claim::Refuse(liveness) => {
                let record = existing.as_ref().map(|r| r.owner.pid).unwrap_or_default();
                return Err(format!(
                    "a zirv runtime is already serving namespace '{namespace_name}' \
                     (pid {record}, {}); attach to it with `zirv session attach`, or stop it \
                     with `zirv session stop --runtime`",
                    liveness.label()
                )
                .into());
            }
            Claim::Replace(_) => namespace::remove(&state, namespace_name),
            Claim::Free => {}
        }
        clear_shutdown(&state, namespace_name);

        let instance = uuid::Uuid::new_v4().to_string();
        let host = RuntimeHost::new(
            state.clone(),
            namespace_name,
            &instance,
            cfg.session.scrollback_rows_or_default(),
            cfg.session.history,
        );
        let native = NativeSessions::new(state.clone(), namespace_name, &instance, environment)?;
        let source = HostSource::new(Arc::clone(&host), Arc::clone(&native), state.clone());
        let server = ApiServer::new(Box::new(source), None);
        // Before the listener binds, so no connection can ever be told a
        // different capability set than the one this server will honour.
        server.attach_host(Arc::clone(&host) as Arc<dyn SessionHost>);
        server.attach_native(Arc::clone(&native) as Arc<dyn NativeHost>);
        let endpoint = super::super::api::server::endpoint_for(&state);
        let running = RunningServer::start(&endpoint, server)?;

        let record = namespace::new_record(
            namespace_name,
            &endpoint.display(),
            now,
            cfg.session.history,
        );
        let record = Namespace {
            instance: instance.clone(),
            ..record
        };
        namespace::write(&state, &record)?;

        Ok(Self {
            state,
            namespace: namespace_name.to_string(),
            instance,
            host,
            native,
            cfg: cfg.clone(),
            advised: std::sync::Mutex::new(std::collections::HashMap::new()),
            running,
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.running.endpoint()
    }

    pub fn host(&self) -> &Arc<RuntimeHost> {
        &self.host
    }

    pub fn native(&self) -> &Arc<NativeSessions> {
        &self.native
    }

    /// One mail sweep across everything this runtime owns. Public so a test
    /// can drive it deterministically rather than waiting for a heartbeat.
    pub fn deliver_mail(&self) {
        let mut advised = match self.advised.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut errors = super::super::dash::ErrorLog::default();
        self.host
            .deliver_mail(&self.cfg, &mut advised, &mut errors);
        self.native.deliver_mail(&self.cfg, &mut errors);
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Tier 2. Relaunches only the topology entries with a verified
    /// conversation reference, each under a NEW session id that records the
    /// old one as its predecessor; everything else is reported, never
    /// respawned and never described as having survived.
    /// Tier 2 for native conversations (issue #489, item 6). Separate from
    /// [`Self::restore`] because the two restore genuinely differently: a pty
    /// entry's process is gone and can at best be relaunched, whereas a native
    /// conversation's whole state is in the journal and comes back by being
    /// read -- reconciled, fenced into a new generation, and never replayed.
    pub fn restore_native(&self) -> NativeRestoreReport {
        self.native.restore()
    }

    pub fn restore(&self, cfg: &CtxConfig) -> RestoreReport {
        let mut report = RestoreReport::default();
        let Some(topology) = super::host::read_topology(&self.state, &self.namespace) else {
            return report;
        };
        let (resumable, skipped) = super::host::partition_resumable(&topology);
        report.skipped = skipped;
        for entry in resumable {
            match self.resume_entry(&entry, cfg) {
                Ok(id) => report.resumed.push(id),
                Err(error) => report.failed.push((entry.short.clone(), error.to_string())),
            }
        }
        report
    }

    fn resume_entry(&self, entry: &TopologyEntry, cfg: &CtxConfig) -> CtxResult<String> {
        let argv =
            super::host::resume_argv(entry).ok_or("no verified resume command for this agent")?;
        let cwd = super::host::restore_cwd(entry, &std::env::current_dir()?);
        let session_id = uuid::Uuid::new_v4().to_string();
        let (env, _) = super::super::dash::build_turn_env(
            cfg,
            &self.state,
            &cwd,
            &entry.agent,
            &session_id,
            super::super::adapters::LaunchMode::Interactive,
        );
        self.host.spawn(SpawnSpec {
            session_id: session_id.clone(),
            agent: entry.agent.clone(),
            role: entry.role.clone(),
            cwd: cwd.clone(),
            repo: cwd,
            verb: super::super::sessions::Verb::Chat,
            argv,
            env,
            rows: entry.rows.max(super::host::DEFAULT_ROWS),
            cols: entry.cols.max(super::host::DEFAULT_COLS),
            conversation: entry.conversation.clone(),
            restored_from: Some(entry.session_id.clone()),
        })
    }

    /// One pass of the service's own work: drain every pty, refresh the
    /// server's session view, and heartbeat the namespace record. Separated
    /// from [`Self::serve`] so a test can drive the loop deterministically
    /// instead of sleeping.
    pub fn tick(&self, heartbeat: bool) {
        self.host.pump();
        // Issue #489 (issue #352's mail-injection residual): the service
        // delivers mail to its OWN sessions, attached or not. On the
        // heartbeat rather than every 25 ms pump, because a mailbox scan is a
        // directory read and a queue nobody is watching is not a hot path.
        if heartbeat {
            self.deliver_mail();
        }
        if heartbeat {
            namespace::touch(&self.state, &self.namespace, state::now_secs());
            self.host.persist_topology();
            self.native.persist_topology();
        }
    }

    /// The serve loop. Returns when the operator requests a shutdown, or when
    /// `until` elapses (the bounded form `zirv session serve --seconds` uses,
    /// and the only form the tests run).
    pub fn serve<W: Write>(&self, w: &mut W, until: Option<Instant>) -> CtxResult<i32> {
        let mut last_heartbeat = Instant::now();
        loop {
            if shutdown_requested(&self.state, &self.namespace) {
                writeln!(w, "zirv session: shutdown requested; draining state")?;
                clear_shutdown(&self.state, &self.namespace);
                return Ok(0);
            }
            if until.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(0);
            }
            let heartbeat = last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL;
            if heartbeat {
                last_heartbeat = Instant::now();
            }
            self.tick(heartbeat);
            std::thread::sleep(PUMP_INTERVAL);
        }
    }

    /// Drains durable state and lets go of the namespace. `stop_sessions`
    /// is the ONLY path that puts a session through the termination ladder,
    /// and it is reached only from an explicit operator request.
    pub fn shutdown(self, stop_sessions: bool) {
        self.host.shutdown(stop_sessions);
        // Issue #489: the native half of the same rule. The topology is
        // drained either way, and a conversation is only ever completed when
        // the operator asked for that -- `--stop-sessions`, or `zirv session
        // stop <id>` one at a time.
        self.native.shutdown(stop_sessions);
        namespace::remove(&self.state, &self.namespace);
        clear_shutdown(&self.state, &self.namespace);
        drop(self.running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: u32, start: Option<u64>) -> Namespace {
        Namespace {
            name: namespace::DEFAULT_NAMESPACE.to_string(),
            owner: namespace::Owner {
                pid,
                start,
                user: None,
            },
            version: "4.8.0".to_string(),
            protocol: 1,
            endpoint: "<state>/s/api.sock".to_string(),
            created_at: 100,
            last_client_at: 100,
            instance: "inst-1".to_string(),
            history: false,
        }
    }

    fn probe(alive: bool, start: Option<u64>) -> impl Fn(u32) -> ProcessIdentity {
        move |_| ProcessIdentity { alive, start }
    }

    /// The headline lifecycle rule: a namespace whose publisher is still
    /// running is refused, and one whose pid has been RECYCLED is not -- the
    /// distinction a pid-only check cannot make, and the one that decides
    /// whether a crashed runtime blocks its own successor forever.
    #[test]
    fn a_live_runtime_is_refused_and_a_recycled_pid_is_replaceable() {
        let live = record(4242, Some(1_000));
        assert_eq!(
            claim_for(Some(&live), &probe(true, Some(1_000)), 1_100, 120),
            Claim::Refuse(Liveness::Live)
        );
        assert_eq!(
            claim_for(Some(&live), &probe(true, Some(90_000)), 90_100, 120),
            Claim::Replace(Liveness::Recycled)
        );
        assert_eq!(
            claim_for(Some(&live), &probe(false, None), 1_100, 120),
            Claim::Replace(Liveness::Gone)
        );
        assert_eq!(
            claim_for(None, &probe(false, None), 1_100, 120),
            Claim::Free
        );
    }

    /// A record with no start identity and a quiet heartbeat is `Unverified`,
    /// which this layer treats as REFUSE: inventing permission to take over a
    /// namespace nothing could prove was dead is how two runtimes end up
    /// owning the same sessions.
    #[test]
    fn an_unverifiable_record_is_refused_rather_than_taken_over() {
        let unstamped = record(4242, None);
        assert_eq!(
            claim_for(Some(&unstamped), &probe(true, None), 1_000_000, 120),
            Claim::Refuse(Liveness::Unverified)
        );
    }

    #[cfg(windows)]
    fn marker_argv(marker: &str) -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            format!("echo {marker} & ping -n 60 127.0.0.1 >nul"),
        ]
    }

    #[cfg(unix)]
    fn marker_argv(marker: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("echo {marker}; sleep 60"),
        ]
    }

    fn spec(id: &str, cwd: &std::path::Path) -> SpawnSpec {
        SpawnSpec {
            session_id: id.to_string(),
            agent: "claude".to_string(),
            role: "orchestrator".to_string(),
            cwd: cwd.to_path_buf(),
            repo: cwd.to_path_buf(),
            verb: super::super::super::sessions::Verb::Chat,
            argv: marker_argv("ZIRVSERVED"),
            env: Vec::new(),
            rows: 24,
            cols: 80,
            conversation: Some("conv-1".to_string()),
            restored_from: None,
        }
    }

    fn config(repo: &std::path::Path) -> CtxConfig {
        CtxConfig::load(repo, &|_| None).expect("config")
    }

    /// End to end over the REAL transport: a served runtime advertises the
    /// attachment capability, and a client that negotiated it can attach,
    /// read the screen, detach and stop -- all through protocol v1 methods,
    /// with no private message anywhere.
    #[test]
    fn a_served_runtime_serves_the_attachment_surface_over_the_real_transport() {
        use super::super::super::api::client::Client;
        use super::super::super::api::wire::{Capability, Method};
        use serde_json::json;

        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = config(tmp.path());
        let service = RuntimeService::start(state, "default", &cfg).expect("start");

        let id = "aaaaaaaa-2222-4333-8444-555555555555";
        service.host().spawn(spec(id, tmp.path())).expect("spawn");
        service.tick(false);

        let mut client = Client::connect(service.endpoint()).expect("connect");
        assert!(
            client.negotiated().has(Capability::SessionAttach),
            "a server that owns terminals advertises the attachment surface"
        );
        let snapshot = client
            .call(Method::SessionSnapshot, json!({}))
            .expect("snapshot");
        let sessions = snapshot["sessions"].as_array().expect("sessions");
        assert!(
            sessions.iter().any(|facts| facts["session_id"] == id),
            "the runtime's own sessions are in the protocol's session list: {snapshot}"
        );

        let attached = client
            .call(
                Method::SessionAttach,
                json!({"session_id": id, "client_id": "c1", "mode": "controller",
                       "rows": 30, "cols": 100}),
            )
            .expect("attach");
        assert_eq!(attached["attachment"]["controller"], json!("c1"));

        let screen = client
            .call(
                Method::SessionScreen,
                json!({"session_id": id, "client_id": "c1"}),
            )
            .expect("screen");
        assert_eq!(screen["screen"]["rows"], json!(30));

        let detached = client
            .call(
                Method::SessionDetach,
                json!({"session_id": id, "client_id": "c1"}),
            )
            .expect("detach");
        assert_eq!(
            detached["attachment"]["controller"],
            serde_json::Value::Null
        );
        assert!(
            service
                .host()
                .sessions()
                .iter()
                .all(|facts| facts.state != super::super::super::api::wire::SessionState::Ended),
            "detaching over the wire ends nothing"
        );

        let stopped = client
            .call(Method::SessionStop, json!({"session_id": id}))
            .expect("stop");
        assert_eq!(stopped["stopped"], json!(true));

        service.shutdown(true);
    }

    /// The negotiation rule from the client's side: a build that does not
    /// support the attachment capability disables it LOCALLY -- the call is
    /// refused here, without a round trip -- which is what lets a
    /// previous-minor client talk to this runtime at all.
    #[test]
    fn a_client_that_never_heard_of_attachment_disables_it_locally() {
        use super::super::super::api::client::Client;
        use super::super::super::api::wire::{Capability, Method};
        use serde_json::json;

        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = config(tmp.path());
        let service = RuntimeService::start(state, "default", &cfg).expect("start");

        let mut older = Client::connect_as(
            service.endpoint(),
            &[Capability::SessionRead, Capability::SessionControl],
        )
        .expect("connect");
        assert!(!older.negotiated().has(Capability::SessionAttach));
        assert!(
            older
                .negotiated()
                .server_only
                .contains(&Capability::SessionAttach),
            "and it can say WHY: the server offered something it does not know"
        );
        let refusal = older
            .call(
                Method::SessionAttach,
                json!({"session_id": "x", "client_id": "c1"}),
            )
            .expect_err("disabled locally");
        assert!(
            refusal.to_string().contains("disabled locally"),
            "{refusal}"
        );
        // The read surface both ends DO share still works.
        assert!(older.call(Method::SessionSnapshot, json!({})).is_ok());

        service.shutdown(false);
    }

    /// Tier 2's honesty rule at the service level: a stored topology restores
    /// LAYOUT for everything and RESUMES only what carries a verified
    /// conversation reference. The resumable half is exercised through
    /// `host::resume_argv` rather than by launching a harness -- a test never
    /// starts a real agent.
    #[test]
    fn a_restore_reports_what_it_cannot_resume_instead_of_respawning_it() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = config(tmp.path());
        super::super::host::write_topology(
            &state,
            "default",
            &super::super::host::Topology {
                written: 10,
                instance: "an-older-instance".to_string(),
                sessions: vec![TopologyEntry {
                    session_id: "gone-1".to_string(),
                    short: "gone1111".to_string(),
                    agent: "bash".to_string(),
                    role: "orchestrator".to_string(),
                    cwd: tmp.path().to_string_lossy().into_owned(),
                    rows: 40,
                    cols: 120,
                    conversation: None,
                    instance: "an-older-instance".to_string(),
                }],
            },
        )
        .expect("topology");

        let service = RuntimeService::start(state, "default", &cfg).expect("start");
        let report = service.restore(&cfg);
        assert!(
            report.resumed.is_empty(),
            "an arbitrary process is never claimed to survive"
        );
        assert_eq!(report.skipped.len(), 1);
        // The layout is still there to report and to restore from: rows,
        // cols and cwd all survived the restart.
        assert_eq!((report.skipped[0].rows, report.skipped[0].cols), (40, 120));
        assert_eq!(report.skipped[0].cwd, tmp.path().to_string_lossy());
        assert!(service.host().sessions().is_empty());
        assert_ne!(
            service.instance(),
            "an-older-instance",
            "a new service is a new instance, whatever it restored"
        );
        assert_eq!(service.namespace(), "default");
        service.shutdown(false);
    }

    /// A second service must not take a namespace its owner is still serving.
    #[test]
    fn a_second_service_refuses_a_namespace_the_first_still_owns() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = config(tmp.path());
        let first = RuntimeService::start(state.clone(), "default", &cfg).expect("first");
        let error = RuntimeService::start(state.clone(), "default", &cfg)
            .expect_err("the namespace is taken");
        assert!(error.to_string().contains("already serving"), "{error}");
        first.shutdown(false);
        // Once the owner has let go, the namespace is free again.
        let second = RuntimeService::start(state, "default", &cfg).expect("second");
        second.shutdown(false);
    }

    #[test]
    fn a_shutdown_request_round_trips_through_the_state_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(!shutdown_requested(&state, "default"));
        request_shutdown(&state, "default").expect("request");
        assert!(shutdown_requested(&state, "default"));
        clear_shutdown(&state, "default");
        assert!(!shutdown_requested(&state, "default"));
    }

    /// The native environment a served-runtime test injects: a fixed route
    /// identity and a turn runner that records rather than calls a model.
    /// Everything the test asserts -- the transport, the negotiation, the
    /// seats, the durable cursor -- is real.
    #[derive(Debug, Default)]
    struct RecordingEnvironment {
        runs: std::sync::Mutex<Vec<String>>,
    }

    impl NativeEnvironment for RecordingEnvironment {
        fn route_identity(
            &self,
            _repo: &std::path::Path,
            _route: Option<&str>,
            _role: &str,
        ) -> CtxResult<crate::commands::ctx::runtime::journal::RouteIdentity> {
            use crate::commands::ctx::provider::Protocol;
            use crate::commands::ctx::runtime::fixture::fixture_target;

            let target = fixture_target(Protocol::AnthropicMessages, "test-model");
            Ok(crate::commands::ctx::runtime::journal::RouteIdentity {
                route: target.route.clone(),
                provider: target.provider.clone(),
                endpoint: target.endpoint.clone(),
                account: target.account.clone(),
                billing_pool: target.billing_pool.clone(),
                protocol: target.protocol,
                model: target.model.clone(),
            })
        }

        fn run(&self, turn: &crate::commands::ctx::session::native::QueuedTurn) -> CtxResult<()> {
            match self.runs.lock() {
                Ok(mut runs) => runs.push(turn.session.clone()),
                Err(poisoned) => poisoned.into_inner().push(turn.session.clone()),
            }
            Ok(())
        }
    }

    /// Issue #489, criteria 1, 2, 3 and 6, end to end over the REAL transport
    /// (a named pipe on Windows, a unix domain socket elsewhere): one runtime,
    /// one endpoint, one protocol, carrying a NATIVE conversation.
    ///
    /// Start it, negotiate the native capability, submit twice with one
    /// idempotency key, page the durable journal by cursor, attach an observer
    /// and prove it cannot drive the session, then detach and prove the
    /// conversation and its registry record survive that.
    #[test]
    fn a_served_runtime_serves_the_native_surface_over_the_real_transport() {
        use crate::commands::ctx::api::client::Client;
        use crate::commands::ctx::api::wire::{Capability, Method};
        use serde_json::json;

        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = config(tmp.path());
        let environment = Arc::new(RecordingEnvironment::default());
        let service = RuntimeService::start_with(
            state,
            "default",
            &cfg,
            Arc::clone(&environment) as Arc<dyn NativeEnvironment>,
        )
        .expect("start");
        service.native().run_turns_inline_for_test();

        let mut client = Client::connect(service.endpoint()).expect("connect");
        assert!(
            client.negotiated().has(Capability::SessionNative),
            "a runtime that owns conversations advertises the native surface"
        );

        let started = client
            .call(
                Method::SessionStart,
                json!({
                    "runtime": "native",
                    "role": "orchestrator",
                    "cwd": tmp.path().to_string_lossy(),
                    "prompt": "first"
                }),
            )
            .expect("start a native session");
        let id = started["session"]["session_id"]
            .as_str()
            .expect("session id")
            .to_string();
        assert_eq!(started["session"]["runtime"], json!("native"));

        // The session list a client sees is ONE list: the native conversation
        // is in it alongside whatever terminals the runtime holds.
        let snapshot = client
            .call(Method::SessionSnapshot, json!({}))
            .expect("snapshot");
        assert!(
            snapshot["sessions"]
                .as_array()
                .expect("sessions")
                .iter()
                .any(|facts| facts["session_id"] == json!(id)),
            "{snapshot}"
        );

        // Durable idempotency across a retry, proven by the journal rather
        // than by the reply.
        let first = client
            .call_with_key(
                Method::SessionSendInput,
                json!({"session_id": id, "input": "again"}),
                Some("wire-key-1"),
            )
            .expect("submit");
        assert_eq!(first["duplicate"], json!(false));
        let retry = client
            .call_with_key(
                Method::SessionSendInput,
                json!({"session_id": id, "input": "again"}),
                Some("wire-key-1"),
            )
            .expect("retry");
        assert_eq!(retry["message_id"], first["message_id"]);

        let history = client
            .call(Method::SessionHistory, json!({"session_id": id}))
            .expect("history");
        let inputs = history["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter(|entry| entry["role"] == json!("user"))
            .count();
        assert_eq!(inputs, 2, "the launch prompt and one input: {history}");

        // The durable cursor, over the wire.
        let page = client
            .call(
                Method::SessionJournal,
                json!({"session_id": id, "after_sequence": 0, "limit": 1}),
            )
            .expect("journal");
        assert_eq!(page["page"]["events"].as_array().expect("events").len(), 1);
        assert_eq!(page["page"]["gap"], json!(false));
        let cursor = page["page"]["cursor"].as_u64().expect("cursor");
        assert!(cursor > 0);

        // An observer attaches and is refused every mutation.
        client
            .call(
                Method::SessionAttach,
                json!({"session_id": id, "client_id": "watcher", "mode": "observer"}),
            )
            .expect("attach as observer");
        let denied = client
            .call(
                Method::SessionSendInput,
                json!({"session_id": id, "input": "no", "client_id": "watcher"}),
            )
            .expect_err("an observer may not drive the session");
        assert!(denied.to_string().contains("denied"), "{denied}");

        // Detaching is not stopping: the conversation and the registry record
        // the service holds for it both survive.
        client
            .call(
                Method::SessionDetach,
                json!({"session_id": id, "client_id": "watcher"}),
            )
            .expect("detach");
        assert!(
            crate::commands::ctx::sessions::list(&StateDir::from_root(tmp.path().to_path_buf()))
                .iter()
                .any(|(record, _)| record.session == id),
            "a detached native conversation keeps the registry record every policy reads"
        );
        let after_detach = client
            .call(Method::SessionGet, json!({"session_id": id}))
            .expect("get");
        assert_eq!(after_detach["session"]["state"], json!("idle"));

        service.shutdown(false);
    }
}
