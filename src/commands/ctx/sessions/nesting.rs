//! Supervision environment scrubbing and interactive nesting guards.

use super::*;

/// The environment variables that carry one supervised session's *identity*
/// into everything it spawns: which session id turn signals should claim,
/// which socket to post them on, and which transcript file the supervisor is
/// watching. A child that inherits these from an outer session reports its
/// own turns as if they belonged to that outer session -- which is exactly
/// how a nested launch drove the outer rot engine to a `Restart` verdict and
/// had it kill the outer agent (see `nested_session_evidence`).
///
/// Every supervisor scrubs all three off a child command builder before
/// setting whichever of them it actually owns, so "no socket of my own"
/// degrades to *unsupervised*, never to *supervised by somebody else*.
/// `SEAT_MODEL_ENV` rides along for the same reason: it names *this*
/// session's seat, and a worker that inherits an orchestrator's copy would
/// have its own subagent dispatches refused by a guard describing a seat it
/// is not sitting in. `HEADLESS_ENV` rides along for the mirror-image
/// reason: it is proof THIS launch is a headless worker, and an interactive
/// session (`wrap`, `chat`, a dashboard pane) that inherited it from
/// whatever spawned it would wrongly refuse its own interactive `brainstorm`
/// step.
///
/// Issue #249: `PARENT_SESSION_ENV` rides along too -- it names the session
/// THIS one's own env says spawned it, and a child that inherited a copy
/// unscrubbed would see its grandparent's id instead of never having one of
/// its own set at all (see `agent::parent_session_env`'s own doc comment for
/// the same rule at the fold that sets it fresh).
///
/// Issues #328/#334: `SEAT_ROLE_ENV` rides along for the same reason
/// `SEAT_MODEL_ENV` does -- it names *this* session's own seat role, and a
/// worker that inherited an orchestrator's copy would be mistaken for the
/// seat it is not sitting in.
pub const SUPERVISION_ENV: [&str; 11] = [
    super::adapters::SESSION_ENV,
    super::adapters::SOCKET_ENV,
    super::adapters::SEAT_MODEL_ENV,
    super::adapters::SEAT_ROLE_ENV,
    super::adapters::PROXY_DECIDED_ENV,
    super::wrap::TRANSCRIPT_ENV,
    super::adapters::LAUNCH_MODE_ENV,
    super::agent::PARENT_SESSION_ENV,
    super::adapters::HEADLESS_ENV,
    super::agent::RESULT_SCHEMA_ENV,
    super::agent::RESULT_WORKDIR_ENV,
];

/// `portable_pty::CommandBuilder::new` seeds itself from `std::env::vars_os`,
/// so an unset key on the builder still means "inherit". Only an explicit
/// `env_remove` actually keeps the value out of the child.
pub fn scrub_supervision_env(builder: &mut portable_pty::CommandBuilder) {
    for key in SUPERVISION_ENV {
        builder.env_remove(key);
    }
}

/// The `std::process::Command` counterpart, for the headless supervisors.
pub fn scrub_supervision_env_cmd(command: &mut std::process::Command) {
    for key in SUPERVISION_ENV {
        command.env_remove(key);
    }
}

/// Set to `true` to bypass the interactive nesting guard, for the operator
/// who genuinely means to run a session inside a session. Mirrored by
/// `--allow-nested` on `wrap` and `chat`.
pub const ALLOW_NESTED_ENV: &str = "ZIRV_ALLOW_NESTED";

/// Claude Code exports both of these into every process it spawns; either one
/// alone is too weak to key on (`CLAUDECODE` is a plain flag a user could
/// export by hand), so the pair is required together.
const CLAUDE_PID_ENV: &str = "CLAUDE_PID";
const CLAUDE_CODE_ENV: &str = "CLAUDECODE";

/// Why this process looks like it is already running *inside* an agent
/// session, or `None` when nothing says so. Reads the caller's `EnvLookup`
/// only, never the process environment -- and the filesystem only to ask
/// whether the one directory-valued piece of evidence
/// (`DASH_REQUESTS_ENV`) still exists (O5, below).
///
/// Interactive supervision nested inside an existing session is not merely
/// redundant, it is destructive. The nested `wrap` binds its own turn-signal
/// socket, but when that bind fails it still spawns a child -- and that child
/// inherits the *outer* `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET`, so its hooks
/// post phantom turns into the outer supervisor's rot engine until the outer
/// engine verdicts `Restart` and kills its own child: the session the user
/// was actually talking to. `SUPERVISION_ENV` scrubbing closes the inherit
/// half of that; this closes the "should we be here at all" half.
/// Whether the dashboard that owns `requests_dir` is still alive, per its
/// `owner.pid` file. The pidfile lives in the requests dir's PARENT (i.e.
/// `<state>/dash/<short>-<token>/owner.pid`) and holds the dashboard's pid as
/// decimal ASCII. A missing, unreadable, unparseable, or dead-pid pidfile all
/// mean "no live dashboard" -- so an abnormally-exited dashboard's leftover
/// requests directory never wedges a future interactive launch. Only a
/// readable pidfile naming a live process counts.
///
/// `pub(crate)` (issue #144): also the liveness half of `agent::
/// try_join_dashboard`'s own gate, so the two readers of `DASH_REQUESTS_ENV`
/// cannot drift on what "live" means the way they did before -- this guard
/// used to be the only one of the two that checked `owner.pid` at all, so a
/// dashboard that exited abnormally left a directory `try_join_dashboard`
/// still treated as a live channel: a request was written into it, nobody
/// was listening, and the caller burned the whole ack timeout finding that
/// out.
///
/// A thin `bool` projection of [`dashboard_owner_liveness`] -- see that
/// function's own doc comment for the reason `try_join_dashboard` needs
/// instead of just this yes/no answer (fix round 1, both reviewers: a silent
/// refusal here is undiagnosable, the same complaint issue #144's own
/// acceptance criteria raised about the three "dashboard did not answer"
/// messages).
pub(crate) fn dashboard_owner_is_live(requests_dir: &Path) -> bool {
    matches!(dashboard_owner_liveness(requests_dir), OwnerLiveness::Live)
}

/// Why [`dashboard_owner_is_live`] answered the way it did for `requests_dir`
/// -- the same three-way distinction its own doc comment already draws
/// ("missing, unreadable, unparseable, or dead-pid... all mean 'no live
/// dashboard'"), just not collapsed to a bool: `agent::try_join_dashboard`
/// needs the reason to report ("dead owner pid N" vs "missing owner.pid") to
/// an operator who would otherwise see this refusal in total silence.
/// `Missing` folds the unreadable and unparseable cases in with a genuinely
/// absent file -- all three mean the same thing to a caller reporting this
/// upward: no pid was ever recorded to check liveness against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerLiveness {
    Live,
    Missing,
    Dead(u32),
}

pub(crate) fn dashboard_owner_liveness(requests_dir: &Path) -> OwnerLiveness {
    if !requests_dir.is_dir() {
        return OwnerLiveness::Missing;
    }
    let Some(parent) = requests_dir.parent() else {
        return OwnerLiveness::Missing;
    };
    let Ok(contents) = std::fs::read_to_string(parent.join("owner.pid")) else {
        return OwnerLiveness::Missing;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return OwnerLiveness::Missing;
    };
    if is_alive(pid) {
        OwnerLiveness::Live
    } else {
        OwnerLiveness::Dead(pid)
    }
}

pub fn nested_session_evidence(env: super::config::EnvLookup<'_>) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    if let Some(id) = non_empty(env(super::adapters::SESSION_ENV)) {
        found.push(format!(
            "{}={}",
            super::adapters::SESSION_ENV,
            short_id(&id)
        ));
    }
    if non_empty(env(super::adapters::SOCKET_ENV)).is_some() {
        found.push(format!("{} is set", super::adapters::SOCKET_ENV));
    }
    // A dashboard pane's own child inherits this (the dashboard exports it
    // into every pane's turn_env, never into its own process environment --
    // see `dash::run_dashboard`), so a set value here means a dashboard pane
    // owns this terminal, and starting another interactive supervisor (or
    // dashboard) inside it is exactly the nested-session hazard this guard
    // exists to catch. Deliberately not added to `SUPERVISION_ENV`: a pane
    // child's own further children (e.g. a nested `zirv ctx agent`) must
    // still be able to reach the same spawn-request channel, which scrubbing
    // it there would break.
    //
    // O5: the directory has to still exist, exactly as `agent::
    // try_join_dashboard` requires before it will use the channel. The
    // dashboard removes it on quit, so a shell that survived one -- a pane
    // child still sitting at a prompt after the dashboard closed -- carries a
    // stale value naming nothing. Treating that as evidence refused a session
    // no dashboard owns any more, and the two readers of this variable
    // disagreeing about what "set" means was the bug: one channel, one
    // liveness test.
    //
    // A directory alone is not enough, though: an *abnormal* dashboard exit
    // (crash, kill) leaves the directory behind, and a surviving pane shell
    // still carrying this env would then wedge every future interactive
    // launch forever. The dashboard writes its own pid into `owner.pid` (the
    // requests dir's parent, `<state>/dash/<short>-<token>/owner.pid`), so
    // only a pidfile naming a *live* process counts as a dashboard actually
    // owning this terminal -- a stale or dead one is no evidence.
    if non_empty(env(super::dash::spawnreq::DASH_REQUESTS_ENV))
        .is_some_and(|dir| dashboard_owner_is_live(Path::new(&dir)))
    {
        found.push(format!(
            "{} is set (a dashboard pane owns this terminal)",
            super::dash::spawnreq::DASH_REQUESTS_ENV
        ));
    }
    if non_empty(env(CLAUDE_PID_ENV)).is_some() && non_empty(env(CLAUDE_CODE_ENV)).is_some() {
        found.push(format!(
            "{CLAUDE_PID_ENV} and {CLAUDE_CODE_ENV} are set (a Claude Code session owns this terminal)"
        ));
    }
    (!found.is_empty()).then(|| found.join("; "))
}

/// The refusal message an interactive verb prints, or `None` when it may
/// start. `allow_nested` is the verb's own `--allow-nested` flag; the
/// `ZIRV_ALLOW_NESTED` environment variable is the second, equivalent
/// override (strict `true`, matching every other boolean this codebase reads
/// out of the environment).
///
/// Only the interactive verbs (`wrap`, `chat`) call this. Headless workers
/// (`exec`, `loop`, `agent`) legitimately run inside a session -- delegating
/// to one is the whole point of `zirv ctx agent` -- and they never take over
/// the shared console, so they are deliberately not gated.
pub fn nesting_refusal(
    verb: &str,
    env: super::config::EnvLookup<'_>,
    allow_nested: bool,
) -> Option<String> {
    let overridden = allow_nested
        || non_empty(env(ALLOW_NESTED_ENV))
            .is_some_and(|v| v.to_ascii_lowercase().parse::<bool>() == Ok(true));
    if overridden {
        return None;
    }
    let evidence = nested_session_evidence(env)?;
    Some(format!(
        "zirv ctx {verb}: refusing to start inside an existing agent session ({evidence}). \
         A nested interactive session can post turn signals into the outer supervisor and \
         get the outer session compacted, restarted or killed. Run it from a plain terminal, \
         or pass --allow-nested (or set {ALLOW_NESTED_ENV}=true) to override."
    ))
}

#[cfg(test)]
mod tests {
    use super::super::testenv::dead_pid;
    use super::super::tests::env_map;
    use super::*;

    // F2: the nesting guard.

    #[test]
    fn nested_session_evidence_names_every_signal_it_found() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        assert_eq!(
            nested_session_evidence(&|k| empty.get(k).cloned()),
            None,
            "a plain terminal is not nested"
        );

        let env = env_map(&[(
            super::super::adapters::SESSION_ENV,
            "abcdef12-3456-4789-8abc-def012345678",
        )]);
        let evidence = nested_session_evidence(&|k| env.get(k).cloned()).expect("nested");
        assert!(evidence.contains("ZIRV_CTX_SESSION"), "got {evidence}");
        assert!(
            evidence.contains("abcdef12"),
            "names the outer session: {evidence}"
        );

        let env = env_map(&[(super::super::adapters::SOCKET_ENV, "/tmp/sock")]);
        let evidence = nested_session_evidence(&|k| env.get(k).cloned()).expect("nested");
        assert!(evidence.contains("ZIRV_CTX_SOCKET"), "got {evidence}");

        // Either Claude Code marker alone is too weak; the pair is not.
        let only_flag = env_map(&[("CLAUDECODE", "1")]);
        assert_eq!(
            nested_session_evidence(&|k| only_flag.get(k).cloned()),
            None
        );
        let pair = env_map(&[("CLAUDECODE", "1"), ("CLAUDE_PID", "4242")]);
        let evidence = nested_session_evidence(&|k| pair.get(k).cloned()).expect("nested");
        assert!(evidence.contains("Claude Code"), "got {evidence}");

        // An exported-but-empty variable is not evidence of anything.
        let blank = env_map(&[(super::super::adapters::SESSION_ENV, "  ")]);
        assert_eq!(nested_session_evidence(&|k| blank.get(k).cloned()), None);
    }

    /// A pane's own child inherits `DASH_REQUESTS_ENV` from the dashboard
    /// that spawned it (see `dash::run_dashboard`'s own turn_env assembly);
    /// this pins that the guard actually fires on it, the same as it does
    /// for `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET` above. The dashboard's own
    /// startup never has this set in its own process environment -- it only
    /// ever exports it into a pane's turn_env, never its own -- so this is
    /// evidence a *pane* owns the terminal, never a self-trip on the
    /// dashboard's own launch.
    #[test]
    fn dash_requests_env_trips_the_nested_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let requests = tmp
            .path()
            .join("dash")
            .join("aaaa1111-0123")
            .join("requests");
        std::fs::create_dir_all(&requests).expect("mkdir");
        // A live dashboard writes its own pid into `owner.pid`; this test
        // process stands in for that live dashboard.
        std::fs::write(
            requests.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        let dir = requests.display().to_string();
        let env = env_map(&[(
            super::super::dash::spawnreq::DASH_REQUESTS_ENV,
            dir.as_str(),
        )]);
        let evidence =
            nested_session_evidence(&|k| env.get(k).cloned()).expect("a pane owns this terminal");
        assert!(
            evidence.contains(super::super::dash::spawnreq::DASH_REQUESTS_ENV),
            "got {evidence}"
        );
        assert!(evidence.contains("dashboard pane"), "got {evidence}");
    }

    /// O5: the dashboard removes its request directory on quit, so a shell
    /// that outlived one carries a value naming nothing. `agent::
    /// try_join_dashboard` has always required the directory to exist before
    /// it will use the channel; this guard must agree, or a survivor process
    /// is refused an interactive session on the strength of a dashboard that
    /// is gone.
    #[test]
    fn a_stale_dash_requests_path_is_not_evidence_of_a_live_dashboard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let gone = tmp
            .path()
            .join("dash")
            .join("aaaa1111-0123")
            .join("requests")
            .display()
            .to_string();
        let env = env_map(&[(
            super::super::dash::spawnreq::DASH_REQUESTS_ENV,
            gone.as_str(),
        )]);
        assert_eq!(nested_session_evidence(&|k| env.get(k).cloned()), None);
    }

    /// MED (read side of the leaked-spawn-request-dir wedge): a requests
    /// directory that still exists is evidence a dashboard owns the terminal
    /// only when its `owner.pid` names a live process. Missing, or naming a
    /// dead pid, is no evidence -- an abnormally-exited dashboard must not
    /// wedge every future interactive launch.
    #[test]
    fn only_a_live_dashboard_owner_pidfile_counts_as_a_dashboard_owner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let make = |name: &str| {
            let requests = tmp.path().join("dash").join(name).join("requests");
            std::fs::create_dir_all(&requests).expect("mkdir");
            requests
        };
        let env_for = |requests: &Path| {
            env_map(&[(
                super::super::dash::spawnreq::DASH_REQUESTS_ENV,
                requests.to_str().expect("utf8"),
            )])
        };

        // Missing owner.pid: a directory alone is no evidence.
        let missing = make("aaaa1111-0001");
        let env = env_for(&missing);
        assert_eq!(
            nested_session_evidence(&|k| env.get(k).cloned()),
            None,
            "a requests dir with no owner.pid does not wedge the terminal"
        );

        // owner.pid naming a dead process: a crashed dashboard is no evidence.
        let dead = make("bbbb2222-0002");
        std::fs::write(
            dead.parent().expect("parent").join("owner.pid"),
            dead_pid().to_string(),
        )
        .expect("write owner.pid");
        let env = env_for(&dead);
        assert_eq!(
            nested_session_evidence(&|k| env.get(k).cloned()),
            None,
            "a dead dashboard's leftover pidfile does not wedge the terminal"
        );

        // owner.pid naming a live process (this one): a real dashboard owns it.
        let live = make("cccc3333-0003");
        std::fs::write(
            live.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        let env = env_for(&live);
        let evidence = nested_session_evidence(&|k| env.get(k).cloned())
            .expect("a live dashboard owner is evidence");
        assert!(evidence.contains("dashboard pane"), "got {evidence}");
    }

    /// Fix round 1 (issue #144): `dashboard_owner_is_live`'s three cases,
    /// exposed directly through `dashboard_owner_liveness` rather than only
    /// through the collapsed bool -- `agent::try_join_dashboard` needs the
    /// `Dead(pid)`/`Missing` distinction to report a refusal it used to give
    /// in total silence.
    #[test]
    fn dashboard_owner_liveness_distinguishes_missing_dead_and_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let make = |name: &str| {
            let requests = tmp.path().join("dash").join(name).join("requests");
            std::fs::create_dir_all(&requests).expect("mkdir");
            requests
        };

        let missing = make("aaaa1111-0001");
        assert_eq!(dashboard_owner_liveness(&missing), OwnerLiveness::Missing);

        let dead = make("bbbb2222-0002");
        let dead_pid_value = dead_pid();
        std::fs::write(
            dead.parent().expect("parent").join("owner.pid"),
            dead_pid_value.to_string(),
        )
        .expect("write owner.pid");
        assert_eq!(
            dashboard_owner_liveness(&dead),
            OwnerLiveness::Dead(dead_pid_value)
        );

        let live = make("cccc3333-0003");
        std::fs::write(
            live.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        assert_eq!(dashboard_owner_liveness(&live), OwnerLiveness::Live);
    }

    #[test]
    fn allow_nested_overrides_the_guard() {
        let env = env_map(&[(
            super::super::adapters::SESSION_ENV,
            "abcdef12-3456-4789-8abc-def012345678",
        )]);
        let lookup = |k: &str| env.get(k).cloned();
        assert!(
            nesting_refusal("chat", &lookup, false).is_some(),
            "nested by default"
        );
        assert_eq!(
            nesting_refusal("chat", &lookup, true),
            None,
            "--allow-nested is an override"
        );

        let env = env_map(&[
            (
                super::super::adapters::SESSION_ENV,
                "abcdef12-3456-4789-8abc-def012345678",
            ),
            (ALLOW_NESTED_ENV, "true"),
        ]);
        assert_eq!(
            nesting_refusal("chat", &|k| env.get(k).cloned(), false),
            None,
            "ZIRV_ALLOW_NESTED=true is the second override"
        );

        // Strict, like every other boolean read out of the environment here.
        let env = env_map(&[
            (
                super::super::adapters::SESSION_ENV,
                "abcdef12-3456-4789-8abc-def012345678",
            ),
            (ALLOW_NESTED_ENV, "maybe"),
        ]);
        assert!(nesting_refusal("chat", &|k| env.get(k).cloned(), false).is_some());
    }

    #[test]
    fn the_refusal_names_the_verb_the_evidence_and_the_override() {
        let env = env_map(&[(super::super::adapters::SOCKET_ENV, "/tmp/sock")]);
        let msg = nesting_refusal("wrap", &|k| env.get(k).cloned(), false).expect("refused");
        assert!(msg.starts_with("zirv ctx wrap:"), "got {msg}");
        assert!(msg.contains("ZIRV_CTX_SOCKET"), "got {msg}");
        assert!(msg.contains("--allow-nested"), "got {msg}");
        assert!(msg.contains(ALLOW_NESTED_ENV), "got {msg}");
    }

    // F3: no child ever inherits another session's identity.

    #[test]
    fn scrubbing_removes_every_supervision_variable_from_a_pty_builder() {
        let mut builder = portable_pty::CommandBuilder::new("echo");
        for key in SUPERVISION_ENV {
            builder.env(key, "inherited-from-the-outer-session");
            assert!(builder.get_env(key).is_some(), "sanity: {key} was set");
        }
        scrub_supervision_env(&mut builder);
        for key in SUPERVISION_ENV {
            assert_eq!(builder.get_env(key), None, "{key} must not reach the child");
        }
    }

    #[test]
    fn scrubbing_removes_every_supervision_variable_from_a_process_command() {
        let mut command = std::process::Command::new("echo");
        scrub_supervision_env_cmd(&mut command);
        let removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str())
            .collect();
        for key in SUPERVISION_ENV {
            assert!(removed.contains(&key), "{key} must be removed: {removed:?}");
        }
    }
}
