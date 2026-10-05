//! Session screening, workflow, conversation, and interrupted-turn bookkeeping.

use super::*;

/// Keep screening in a non-json sibling file: registry listings scan json,
/// and unrelated hook writes must never race guard whole-record writes. (#243)
pub(super) fn screening_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.screening"))
}

#[derive(Debug, Serialize, Deserialize)]
struct ScreeningSummary {
    summary: String,
}

/// Store screening atomically without touching the registry record.
pub fn set_last_screening(state: &StateDir, short: &str, summary: Option<String>) {
    let path = screening_path(state, short);
    match summary {
        Some(summary) if !summary.is_empty() => {
            let _ = super::state::create_private_dir_all(&state.sessions());
            if let Ok(json) = serde_json::to_string(&ScreeningSummary { summary }) {
                let _ = super::state::write_private(&path, &json);
            }
        }
        _ => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Return the stored screening summary, or none when absent or unreadable.
pub fn last_screening(state: &StateDir, short: &str) -> Option<String> {
    let text = std::fs::read_to_string(screening_path(state, short)).ok()?;
    serde_json::from_str::<ScreeningSummary>(&text)
        .ok()
        .map(|s| s.summary)
}

/// Persist screening as a side channel without changing rot verdicts;
/// announce changed summaries once per supervised run. (#243)
pub fn record_screening(
    state: &StateDir,
    short: &str,
    report: &super::screen::ScreenReport,
    announcer: &super::announce::Announcer,
    last_announced: &mut Option<String>,
) -> bool {
    let summary = (!report.is_clean()).then(|| report.summary());
    set_last_screening(state, short, summary.clone());
    let announce = summary.is_some() && summary != *last_announced;
    if announce {
        announcer.emit(&super::announce::Event::Screening {
            summary: summary.clone().unwrap_or_default(),
        });
    }
    *last_announced = summary;
    announce
}

/// Keep workflow binding in a sibling file so another process cannot race
/// the guard's whole-record writes or lose the binding.
pub(super) fn workflow_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.workflow"))
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowBinding {
    workflow_id: String,
}

/// Bind only an already registered session, atomically and best-effort.
pub fn bind_workflow_id(state: &StateDir, short: &str, workflow_id: &str) {
    if load_record(state, short).is_none() {
        return;
    }
    let path = workflow_path(state, short);
    let _ = super::state::create_private_dir_all(&state.sessions());
    if let Ok(json) = serde_json::to_string(&WorkflowBinding {
        workflow_id: workflow_id.to_string(),
    }) {
        let _ = super::state::write_private(&path, &json);
    }
}

/// Read the bound workflow independently of registry rewrites.
pub fn workflow_id_for(state: &StateDir, short: &str) -> Option<String> {
    let text = std::fs::read_to_string(workflow_path(state, short)).ok()?;
    serde_json::from_str::<WorkflowBinding>(&text)
        .ok()
        .map(|b| b.workflow_id)
}

/// Every `(short session id, workflow id)` binding on disk, read-only; unreadable entries are skipped.
pub fn workflow_bindings(state: &StateDir) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("workflow") {
                return None;
            }
            let short = path.file_stem()?.to_str()?.to_string();
            let id = workflow_id_for(state, &short)?;
            Some((short, id))
        })
        .collect()
}

/// Consume one dead session's in-flight witness without sweeping other
/// records; malformed state is ignored and a failed clear may repeat it. (#281)
pub fn take_interrupted_in_flight(state: &StateDir, repo: &Path) -> Option<InFlight> {
    interrupted_in_flight(state, repo, true)
}

/// Read a crash witness without consuming it for prompt previews.
pub fn peek_interrupted_in_flight(state: &StateDir, repo: &Path) -> Option<InFlight> {
    interrupted_in_flight(state, repo, false)
}

fn interrupted_in_flight(state: &StateDir, repo: &Path, consume: bool) -> Option<InFlight> {
    let (path, mut record) = interrupted_record(state, repo)?;
    let in_flight = record.in_flight.take()?;
    if consume && let Ok(json) = serde_json::to_string_pretty(&record) {
        let _ = super::state::write_private(&path, &json);
    }
    Some(in_flight)
}

/// Consume the crash witness at launch, restoring it if spawning/exec fails.
pub(crate) fn launch_consuming_interrupted<T>(
    state: &StateDir,
    repo: &Path,
    launch: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let interrupted = interrupted_record(state, repo);
    if let Some((path, record)) = &interrupted {
        let mut cleared = record.clone();
        cleared.in_flight = None;
        if let Ok(json) = serde_json::to_string_pretty(&cleared) {
            let _ = super::state::write_private(path, &json);
        }
    }
    let result = launch();
    if result.is_err()
        && let Some((path, record)) = interrupted
        && let Ok(json) = serde_json::to_string_pretty(&record)
    {
        let _ = super::state::write_private(&path, &json);
    }
    result
}

fn interrupted_record(state: &StateDir, repo: &Path) -> Option<(PathBuf, Record)> {
    let repo_slug = super::state::repo_slug(repo);
    let entries = std::fs::read_dir(state.sessions()).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Record>(&contents) else {
            continue;
        };
        if record.repo_slug != repo_slug || record_is_alive(&record) {
            continue;
        }
        if record.in_flight.is_none() {
            continue;
        }
        return Some((path, record));
    }
    None
}

/// Sweep screening siblings whose session record is no longer live. (#243)
pub(super) fn sweep_orphaned_screening_summaries(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("screening") {
            continue;
        }
        let Some(short) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let has_live_record = found
            .iter()
            .any(|(record, liveness)| *liveness == Liveness::Live && record.short == short);
        if !has_live_record {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Sweep workflow siblings whose session record is no longer live.
pub(super) fn sweep_orphaned_workflow_markers(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("workflow") {
            continue;
        }
        let Some(short) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let has_live_record = found
            .iter()
            .any(|(record, liveness)| *liveness == Liveness::Live && record.short == short);
        if !has_live_record {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Record the harness-minted conversation id because zirv's session id
/// may not be resumable by that harness. (#462)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct NativeConversation {
    /// Harness identity prevents resuming under a later successor.
    agent: String,
    /// Session identity prevents reuse of a stale short-address marker.
    session: String,
    conversation: String,
    /// Backend identity prevents using a harness resume id as a native
    /// journal id; old markers default to Harness. (#470/#488)
    #[serde(default)]
    runtime: RuntimeKind,
}

fn conversation_marker_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.conversation"))
}

/// Update the actual conversation on every turn; the harness can mint a
/// new one mid-session. Failure only limits later recovery.
pub fn record_native_conversation(
    state: &StateDir,
    short: &str,
    agent: &str,
    session: &str,
    conversation: &str,
) {
    record_conversation_on(
        state,
        short,
        agent,
        session,
        conversation,
        RuntimeKind::Harness,
    )
}

/// Record the backend that defines this opaque conversation id. (#488)
pub fn record_conversation_on(
    state: &StateDir,
    short: &str,
    agent: &str,
    session: &str,
    conversation: &str,
    runtime: RuntimeKind,
) {
    if agent.is_empty() || session.is_empty() || conversation.is_empty() {
        return;
    }
    let record = NativeConversation {
        agent: agent.to_string(),
        session: session.to_string(),
        conversation: conversation.to_string(),
        runtime,
    };
    let Ok(body) = serde_json::to_string(&record) else {
        return;
    };
    let _ = super::state::create_private_dir_all(&state.sessions());
    let _ = super::state::write_private(&conversation_marker_path(state, short), &body);
}

/// Return a conversation only when agent, session, and runtime all match;
/// otherwise recovery must relaunch cold instead of guessing. (#470)
pub fn native_conversation(
    state: &StateDir,
    short: &str,
    agent: &str,
    session: &str,
    runtime: RuntimeKind,
) -> Option<String> {
    let body = std::fs::read_to_string(conversation_marker_path(state, short)).ok()?;
    let record: NativeConversation = serde_json::from_str(&body).ok()?;
    (record.agent.eq_ignore_ascii_case(agent)
        && record.session == session
        && record.runtime == runtime)
        .then_some(record.conversation)
        .filter(|conversation| !conversation.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::testenv::dead_pid;
    use super::super::tests::{record_for, state_in};
    use super::*;

    /// Dash refresh PR1: a fresh session starts with no bound workflow, and
    /// an already-registered one picks one up through `bind_workflow_id`'s
    /// own sibling file.
    #[test]
    fn bind_workflow_id_patches_an_existing_sessions_own_sibling_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let record = record_for(
            "22222222-3333-4444-8888-555555555555",
            Path::new("/repo"),
            Verb::Wrap,
        );
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);
        assert_eq!(workflow_id_for(&state, &short), None);

        bind_workflow_id(&state, &short, "w-9c02");
        assert_eq!(workflow_id_for(&state, &short).as_deref(), Some("w-9c02"));
    }

    /// `bind_workflow_id` against a short id with no registered record at all
    /// is a quiet no-op, never a panic -- the same best-effort posture every
    /// other write in this module holds to.
    #[test]
    fn bind_workflow_id_is_a_quiet_no_op_with_no_registered_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        bind_workflow_id(&state, "ghost0001", "w-0000");
        assert!(load_record(&state, "ghost0001").is_none());
        assert_eq!(workflow_id_for(&state, "ghost0001"), None);
    }

    /// Round 2 coordinator review, CONFIRMED severe: a wrap-supervised
    /// session's `SessionGuard` holds its OWN cached `Record` from
    /// `register` time, and rewrites it whole on every turn
    /// (`stamp_in_flight`, `write_record(&self.state, &self.record)`).
    /// When `bind_workflow_id` used to patch that same `record_path` file,
    /// the very next `stamp_in_flight` call reverted the binding to
    /// whatever the guard's own in-memory copy still held (`None`, since it
    /// predates the bind) -- silently, with no error anywhere. Pins the
    /// fix: the binding lives in its own sibling file, which nothing the
    /// guard writes ever touches, so it survives every turn.
    #[test]
    fn bind_workflow_id_survives_the_guards_own_whole_record_rewrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let record = record_for(
            "33333333-4444-4555-8666-777777777777",
            Path::new("/repo"),
            Verb::Wrap,
        );
        let short = record.short.clone();
        let mut guard = SessionGuard::register(&state, record);

        // The bind happens from the SEPARATE `zirv workflow start` process,
        // after the guard already registered -- exactly the ordering that
        // exposed the bug.
        bind_workflow_id(&state, &short, "w-9c02");
        assert_eq!(workflow_id_for(&state, &short).as_deref(), Some("w-9c02"));

        // The guard's own next turn rewrites its whole cached `Record` --
        // the write that used to clobber a record-field binding.
        guard.stamp_in_flight("wrap", 1);

        assert_eq!(
            workflow_id_for(&state, &short).as_deref(),
            Some("w-9c02"),
            "the binding must survive the guard's own whole-record rewrite"
        );
    }

    /// Issue #243 (review round, F1): the whole point of the sibling file --
    /// a screening write must never open, let alone rewrite, `record_path`
    /// at all. Proven at the strongest level available: the record's own
    /// bytes on disk, byte for byte, before and after.
    #[test]
    fn set_last_screening_never_touches_the_record_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = Path::new("/repo");
        let record = record_for("11111111-2222-4333-8444-555555555555", repo, Verb::Wrap);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let before = std::fs::read(record_path(&state, &short)).expect("record written");
        set_last_screening(&state, &short, Some("1 flag: something".to_string()));
        let after = std::fs::read(record_path(&state, &short)).expect("record still there");
        assert_eq!(
            before, after,
            "a screening write must never touch the record file's own bytes"
        );
    }

    /// Issue #462: the marker answers only for the exact (agent, zirv
    /// session) pair that recorded it. A short id is a STABLE address that
    /// outlives both a rollover to another harness and the session that
    /// happened to hold it, so answering for either would hand a recovery
    /// somebody else's conversation.
    #[test]
    fn native_conversation_answers_only_for_the_agent_and_session_that_recorded_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let zirv_session = "6c967beb-0b72-46e9-9d3e-504a03f741b3";
        let native = "49195b07-217f-4401-8681-c857fcea294e";

        record_native_conversation(&state, "orch0001", "claude", zirv_session, native);
        assert_eq!(
            native_conversation(
                &state,
                "orch0001",
                "claude",
                zirv_session,
                RuntimeKind::Harness
            )
            .as_deref(),
            Some(native),
        );
        assert_eq!(
            native_conversation(
                &state,
                "orch0001",
                "Claude",
                zirv_session,
                RuntimeKind::Harness
            )
            .as_deref(),
            Some(native),
            "an agent name differing only in case is the same harness"
        );
        assert_eq!(
            native_conversation(
                &state,
                "orch0001",
                "codex",
                zirv_session,
                RuntimeKind::Harness
            ),
            None,
            "a seat rolled over to another harness must not resume claude's conversation"
        );
        assert_eq!(
            native_conversation(
                &state,
                "orch0001",
                "claude",
                "some-other-session",
                RuntimeKind::Harness
            ),
            None,
            "a marker left by an earlier session at this address is not this one's"
        );
        assert_eq!(
            native_conversation(
                &state,
                "orch0002",
                "claude",
                zirv_session,
                RuntimeKind::Harness
            ),
            None,
            "no marker at all is no answer, never a guess"
        );
    }

    /// Issue #470: a conversation recorded by the harness backend must
    /// never be handed to a caller asking on behalf of the native backend,
    /// even for the exact same agent/session -- the two runtimes' own
    /// conversation ids live in entirely different namespaces.
    #[test]
    fn native_conversation_does_not_answer_for_a_different_runtime() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        record_native_conversation(&state, "orch0004", "claude", "sess", "conv");
        assert_eq!(
            native_conversation(&state, "orch0004", "claude", "sess", RuntimeKind::Harness)
                .as_deref(),
            Some("conv")
        );
        assert_eq!(
            native_conversation(&state, "orch0004", "claude", "sess", RuntimeKind::Native),
            None,
            "a marker the harness backend recorded must not resume the native backend"
        );
    }

    /// An older build's marker file predates the `runtime` field entirely.
    /// It must still parse, and must answer as `Harness` -- the only
    /// runtime that could have written it.
    #[test]
    fn a_marker_without_a_runtime_field_still_parses_as_harness() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let _ = super::super::state::create_private_dir_all(&state.sessions());
        let body = serde_json::json!({
            "agent": "claude",
            "session": "sess",
            "conversation": "conv",
        })
        .to_string();
        super::super::state::write_private(&conversation_marker_path(&state, "orch0005"), &body)
            .expect("write marker");
        assert_eq!(
            native_conversation(&state, "orch0005", "claude", "sess", RuntimeKind::Harness)
                .as_deref(),
            Some("conv"),
            "a pre-#470 marker has no runtime field and must default to Harness"
        );
        assert_eq!(
            native_conversation(&state, "orch0005", "claude", "sess", RuntimeKind::Native),
            None
        );
    }

    /// An empty id is not evidence: recording one must leave nothing behind
    /// for a recovery to read back as a conversation.
    #[test]
    fn record_native_conversation_ignores_empty_identities() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        record_native_conversation(&state, "orch0003", "claude", "sess", "");
        record_native_conversation(&state, "orch0003", "", "sess", "conv");
        assert_eq!(
            native_conversation(&state, "orch0003", "claude", "sess", RuntimeKind::Harness),
            None
        );
    }

    #[test]
    fn last_screening_round_trips_and_clears() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let short = "aaaa1111";
        assert_eq!(last_screening(&state, short), None);

        set_last_screening(&state, short, Some("2 flags: x, y".to_string()));
        assert_eq!(
            last_screening(&state, short).as_deref(),
            Some("2 flags: x, y")
        );

        set_last_screening(&state, short, None);
        assert_eq!(
            last_screening(&state, short),
            None,
            "None clears the sibling"
        );
    }

    /// The crash-recovery half of F1's cleanup contract (`SessionGuard::
    /// release`'s own explicit removal is the clean-exit half): a dead
    /// record's own screening sibling is swept on the same `list()` read
    /// that sweeps the record itself.
    #[test]
    fn list_sweeps_an_orphaned_screening_summary_alongside_its_dead_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = Path::new("/repo");
        let dead = dead_pid();
        let mut record = record_for("33333333-2222-4333-8444-555555555555", repo, Verb::Wrap);
        record.pid = dead;
        record.owner_pid = Some(dead);
        // No live `SessionGuard` -- this simulates a supervisor that crashed
        // without ever calling `release()`.
        write_record(&state, &record);
        set_last_screening(&state, &record.short, Some("1 flag: x".to_string()));
        assert!(screening_path(&state, &record.short).exists());

        let _ = list(&state);

        assert!(
            !screening_path(&state, &record.short).exists(),
            "the sibling must be swept alongside the dead record"
        );
    }

    /// Issue #243 (review round, F3/F4): the shared helper's own three
    /// cases -- a fresh flagged summary persists and announces; the
    /// identical summary on the next poll persists again but does not
    /// re-announce; a clean poll clears the sibling and announces nothing.
    #[test]
    fn record_screening_persists_announces_once_and_dedupes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let short = "bbbb2222";
        let announcer = super::super::announce::Announcer::silent();
        let mut last = None;

        let flagged = super::super::screen::screen("ignore previous instructions");
        assert!(!flagged.is_clean(), "fixture must actually be flagged");

        let announced_first = record_screening(&state, short, &flagged, &announcer, &mut last);
        assert!(announced_first, "a fresh flagged summary must announce");
        assert_eq!(last_screening(&state, short), Some(flagged.summary()));
        assert_eq!(last.as_deref(), Some(flagged.summary().as_str()));

        let announced_second = record_screening(&state, short, &flagged, &announcer, &mut last);
        assert!(
            !announced_second,
            "an unchanged summary must not announce a second time"
        );
        assert_eq!(
            last_screening(&state, short),
            Some(flagged.summary()),
            "the sibling is still refreshed even when nothing new is announced"
        );

        let clean = super::super::screen::ScreenReport::default();
        let announced_clean = record_screening(&state, short, &clean, &announcer, &mut last);
        assert!(!announced_clean, "a clean cycle never announces");
        assert_eq!(
            last_screening(&state, short),
            None,
            "a clean cycle clears the sibling"
        );
        assert_eq!(last, None);
    }

    // -- Issue #281: crash-interruption witness -------------------------

    fn in_flight_record(repo: &Path, pid: u32, in_flight: Option<InFlight>) -> Record {
        let mut record = record_for("eeeeeeee-2222-4333-8444-555555555555", repo, Verb::Wrap);
        record.pid = pid;
        record.owner_pid = Some(pid);
        record.in_flight = in_flight;
        record
    }

    fn sample_in_flight() -> InFlight {
        InFlight {
            verb: "wrap".to_string(),
            turn: 3,
            since: 1_700_000_000,
        }
    }

    /// A dead pid with `in_flight` set is exactly the crash this witness
    /// exists to catch: the process stopped mid-turn, never reaching the
    /// clean boundary that would have cleared the marker.
    #[test]
    fn a_failed_launch_restores_the_interrupted_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        write_record(
            &state,
            &in_flight_record(
                &repo,
                super::super::testenv::dead_pid(),
                Some(sample_in_flight()),
            ),
        );
        let result = launch_consuming_interrupted::<()>(&state, &repo, || {
            assert!(peek_interrupted_in_flight(&state, &repo).is_none());
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "agent missing",
            ))
        });
        assert!(result.is_err());
        assert_eq!(
            peek_interrupted_in_flight(&state, &repo),
            Some(sample_in_flight())
        );
    }

    #[test]
    fn a_successful_launch_consumes_the_interrupted_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        write_record(
            &state,
            &in_flight_record(
                &repo,
                super::super::testenv::dead_pid(),
                Some(sample_in_flight()),
            ),
        );
        launch_consuming_interrupted(&state, &repo, || Ok(())).expect("launch");
        assert!(peek_interrupted_in_flight(&state, &repo).is_none());
    }

    #[test]
    fn a_dead_pid_with_in_flight_set_is_reported_once() {
        use super::super::testenv::dead_pid;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = in_flight_record(&repo, dead_pid(), Some(sample_in_flight()));
        write_record(&state, &record);

        let found = take_interrupted_in_flight(&state, &repo).expect("must report the crash");
        assert_eq!(found.verb, "wrap");
        assert_eq!(found.turn, 3);

        assert!(
            take_interrupted_in_flight(&state, &repo).is_none(),
            "the marker was consumed -- a second call must find nothing"
        );
    }

    /// `resume --print-prompt` peeks: the witness is shown, and the marker is
    /// still there for the real resume that follows.
    #[test]
    fn peeking_leaves_the_marker_for_the_real_resume_to_take() {
        use super::super::testenv::dead_pid;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        write_record(
            &state,
            &in_flight_record(&repo, dead_pid(), Some(sample_in_flight())),
        );

        assert_eq!(
            peek_interrupted_in_flight(&state, &repo).map(|f| f.turn),
            Some(3)
        );
        assert_eq!(
            take_interrupted_in_flight(&state, &repo).map(|f| f.turn),
            Some(3),
            "a peek must not have consumed the marker"
        );
        assert!(take_interrupted_in_flight(&state, &repo).is_none());
    }

    /// A dead pid with no marker at all means a clean exit (or a record from
    /// before this field existed): nothing to report.
    #[test]
    fn a_dead_pid_with_no_marker_reports_nothing() {
        use super::super::testenv::dead_pid;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = in_flight_record(&repo, dead_pid(), None);
        write_record(&state, &record);

        assert!(take_interrupted_in_flight(&state, &repo).is_none());
    }

    /// A live process is never "interrupted", even if its own marker happens
    /// to still be set (it just has not reached the next turn boundary yet).
    #[test]
    fn a_live_pid_with_in_flight_set_reports_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = in_flight_record(&repo, std::process::id(), Some(sample_in_flight()));
        write_record(&state, &record);

        assert!(
            take_interrupted_in_flight(&state, &repo).is_none(),
            "a live session's own in-flight marker is not a crash"
        );
    }

    /// A record for a DIFFERENT repository must never be reported, even if
    /// it is a dead, in-flight one -- `repo_slug` is the match key, the same
    /// one `handoff::store`/`latest_for_repo` already use.
    #[test]
    fn a_dead_in_flight_record_for_a_different_repo_is_not_reported() {
        use super::super::testenv::dead_pid;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let this_repo = tmp.path().join("repo-a");
        let other_repo = tmp.path().join("repo-b");
        let record = in_flight_record(&other_repo, dead_pid(), Some(sample_in_flight()));
        write_record(&state, &record);

        assert!(take_interrupted_in_flight(&state, &this_repo).is_none());
    }

    /// Issue #467 review (defect 2): unlike workflow-state lookup, crash
    /// witnesses must stay keyed by the LITERAL checkout even for a `git
    /// worktree add` sibling of the same repository -- a new session
    /// starting in one worktree must never consume (or even see) a crash
    /// witness left by a session that died in a sibling worktree, since the
    /// two are different processes working on different trees. `repo_slug`
    /// (this module's match key) resolves worktree siblings independently
    /// of `workflow::engine`'s own, deliberately separate, sibling-checkout
    /// lookup.
    #[test]
    fn a_sibling_worktrees_dead_in_flight_record_is_not_reported() {
        use super::super::testenv::dead_pid;
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());

        let main_repo = tmp.path().join("main-repo");
        std::fs::create_dir_all(&main_repo).expect("create main repo dir");
        let git = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&main_repo, &["init", "-q"]);
        std::fs::write(main_repo.join("README.md"), "hello\n").unwrap();
        git(&main_repo, &["add", "."]);
        git(&main_repo, &["commit", "-q", "-m", "base"]);

        let worktree = tmp.path().join("linked-worktree");
        git(
            &main_repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );

        // The crash witness belongs to the linked worktree.
        let record = in_flight_record(&worktree, dead_pid(), Some(sample_in_flight()));
        write_record(&state, &record);

        // A session starting fresh in the main checkout must not see it,
        // even though both share the same `.git` and the same commit
        // history.
        assert!(
            take_interrupted_in_flight(&state, &main_repo).is_none(),
            "a sibling worktree's crash witness must never leak into the main checkout"
        );
        // The witness is still there for the worktree itself, unconsumed.
        assert!(take_interrupted_in_flight(&state, &worktree).is_some());
    }

    /// Back-compat: a `Record` serialized by a build before this field
    /// existed has no `in_flight` key at all in its JSON. It must still
    /// deserialize, with `in_flight` defaulting to `None` -- never a parse
    /// failure that would make the whole record (and therefore the session)
    /// vanish from every listing.
    #[test]
    fn a_record_json_with_no_in_flight_field_deserializes_with_none() {
        let json = r#"{
            "session": "ffffffff-2222-4333-8444-555555555555",
            "short": "ffffffff",
            "agent": "claude",
            "repo": "/repo",
            "repo_slug": "repo",
            "verb": "wrap",
            "pid": 1,
            "started_at": 0,
            "reachable": true
        }"#;
        let record: Record = serde_json::from_str(json).expect("an older record still parses");
        assert_eq!(record.in_flight, None);
    }

    /// `wrap`'s `Input` arm calls `stamp_in_flight` on every keystroke chunk,
    /// so a repeat stamp for the turn already marked must not rewrite the
    /// record: the original `since` survives, and a new turn replaces it.
    #[test]
    fn stamping_the_same_turn_again_does_not_rewrite_the_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = in_flight_record(&repo, std::process::id(), None);
        let mut guard = SessionGuard::register(&state, record);

        guard.stamp_in_flight("wrap", 2);
        guard.record.in_flight.as_mut().expect("stamped").since = 5;
        guard.stamp_in_flight("wrap", 2);
        assert_eq!(guard.record().in_flight.as_ref().map(|f| f.since), Some(5));

        guard.stamp_in_flight("wrap", 3);
        let stamped = guard.record().in_flight.as_ref().expect("restamped");
        assert_eq!(stamped.turn, 3);
        assert_ne!(stamped.since, 5);
        guard.release();
    }
}
