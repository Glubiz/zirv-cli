//! Supervision environment scrubbing and interactive nesting guards.

use super::*;

/// Scrub inherited session, seat, headless, and parent identity before
/// setting this child's own values; failed bind must mean unsupervised,
/// never supervised by an outer session. (#249/#328/#334)
pub const SUPERVISION_ENV: [&str; 12] = [
    super::adapters::SESSION_ENV,
    super::adapters::SOCKET_ENV,
    super::adapters::SEAT_MODEL_ENV,
    super::adapters::SEAT_ROLE_ENV,
    super::adapters::PROXY_DECIDED_ENV,
    super::wrap::TRANSCRIPT_ENV,
    super::adapters::LAUNCH_MODE_ENV,
    super::agent::PARENT_SESSION_ENV,
    super::adapters::HEADLESS_ENV,
    super::adapters::INTERNAL_ENV,
    super::agent::RESULT_SCHEMA_ENV,
    super::agent::RESULT_WORKDIR_ENV,
];

/// CommandBuilder inherits ambient env unless keys are explicitly removed.
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

/// Operator override for intentional interactive nesting.
pub const ALLOW_NESTED_ENV: &str = "ZIRV_ALLOW_NESTED";

/// Require both Claude environment markers; either alone is too weak.
const CLAUDE_PID_ENV: &str = "CLAUDE_PID";
const CLAUDE_CODE_ENV: &str = "CLAUDECODE";

/// Detect an existing interactive owner before launching another supervisor.
/// Dashboard requests directories count only when owner.pid names a live
/// process; stale or malformed state grants no ownership. (#144)
pub(crate) fn dashboard_owner_is_live(requests_dir: &Path) -> bool {
    matches!(dashboard_owner_liveness(requests_dir), OwnerLiveness::Live)
}

/// Preserve why dashboard ownership failed so callers can report it.
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
    // A pane channel may identify its owner, but must stay available to
    // child spawn requests; require a live owner pid before refusing nesting.
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

/// Refuse nesting unless the operator explicitly allows it. Only the
/// interactive verbs (`wrap`, `chat`) call this: headless workers legitimately
/// run inside a session -- delegation is the whole point of `zirv ctx agent`
/// -- and never take over the shared console, so they are deliberately not
/// gated here.
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
