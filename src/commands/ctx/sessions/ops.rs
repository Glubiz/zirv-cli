//! Session nudge and kill operations, delivery filters, and wake/stall markers.

use std::io::{Read, Write};

use super::super::CtxResult;
use super::*;

/// Remove orphan wake-up markers when their session is no longer live.
pub(super) fn sweep_orphaned_markers(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.sessions()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("nudge") {
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

// Nudge stores ordinary mail before writing its wake-up marker.

fn nudge_marker_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.nudge"))
}

/// Report an unusable marker without inventing sender identity.
pub const UNKNOWN_SENDER: &str = "unknown";

/// Marker writes are best-effort state-dir housekeeping.
fn write_nudge_marker(state: &StateDir, short: &str, from: &str) {
    let _ = super::state::create_private_dir_all(&state.sessions());
    let _ = super::state::write_private(&nudge_marker_path(state, short), from);
}

/// Notify only after durable mail storage; notification is best-effort.
pub(crate) fn notify_mail(state: &StateDir, short: &str, from: &str) {
    write_nudge_marker(state, short, from);
}

/// Atomically claim one wake-up marker so it cannot fire twice.
pub fn claim_nudge_marker(state: &StateDir, short: &str) -> Option<String> {
    let path = nudge_marker_path(state, short);
    let contents = std::fs::read_to_string(&path).ok();
    if std::fs::remove_file(&path).is_err() {
        return None;
    }
    Some(
        contents
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| UNKNOWN_SENDER.to_string()),
    )
}

// Stall markers are repeatable observations, unlike claimed nudges. (#310)

fn stall_marker_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.stall"))
}

/// Store stall onset best-effort; in-memory state still controls action.
pub fn write_stall_marker(state: &StateDir, short: &str, latched_at_secs: u64) {
    let _ = super::state::create_private_dir_all(&state.sessions());
    let _ = super::state::write_private(
        &stall_marker_path(state, short),
        &latched_at_secs.to_string(),
    );
}

/// Read stall onset without consuming it for supervisor and dashboard.
pub fn stall_marker(state: &StateDir, short: &str) -> Option<u64> {
    std::fs::read_to_string(stall_marker_path(state, short))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Clear a stall marker when progress resumes, best-effort.
pub fn clear_stall_marker(state: &StateDir, short: &str) {
    let _ = std::fs::remove_file(stall_marker_path(state, short));
}

/// Always filter directed mail by this run's stable registry address;
/// an unfiltered listing could consume another session's messages.
pub fn delivery_filter<'a>(latched: Option<&'a str>, current: &'a str) -> Option<&'a str> {
    Some(latched.unwrap_or(current))
}

/// Require at least four short-id characters for actions; a unique typo
/// can otherwise wake or kill the wrong session.
pub const MIN_TARGET_PREFIX: usize = 4;

/// Refuse short action prefixes except a complete short id.
pub fn prefix_too_short(verb: &str, prefix: &str, live_shorts: &[String]) -> Option<String> {
    if prefix.chars().count() >= MIN_TARGET_PREFIX {
        return None;
    }
    if live_shorts.iter().any(|short| short == prefix) {
        return None;
    }
    let listed = if live_shorts.is_empty() {
        "none registered".to_string()
    } else {
        live_shorts.join(", ")
    };
    Some(format!(
        "prefix too short (a {verb} needs at least {MIN_TARGET_PREFIX} characters, \
         or a session's whole short id); sessions: {listed}"
    ))
}

/// Every live session's short id, in the order `list` found them. The list a
/// refusal names back to the operator.
fn live_shorts(state: &StateDir) -> Vec<String> {
    list(state)
        .into_iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short)
        .collect()
}

#[derive(Debug, clap::Args)]
pub struct NudgeArgs {
    /// Short id (or a unique prefix of one, at least four characters) of the
    /// live session to nudge.
    pub prefix: String,
    /// Message text. When omitted, read from `--message-file`, else from
    /// stdin.
    #[arg(long)]
    pub message: Option<String>,
    /// Path to a file holding the message text.
    #[arg(long)]
    pub message_file: Option<PathBuf>,
}

/// `--message`, else `--message-file`, else stdin -- trimmed either way, the
/// same convention `mail::resolve_message` uses.
fn resolve_nudge_message(args: &NudgeArgs, stdin: &mut dyn Read) -> CtxResult<String> {
    if let Some(text) = &args.message {
        return Ok(text.trim().to_string());
    }
    if let Some(path) = &args.message_file {
        return Ok(std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .trim()
            .to_string());
    }
    let mut buffer = String::new();
    stdin.read_to_string(&mut buffer)?;
    Ok(buffer.trim().to_string())
}

pub fn run_nudge_with<W: Write>(
    args: &NudgeArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    stdin: &mut dyn Read,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    if !cfg.mail.enabled {
        // A nudge needs durable mail before any wake-up marker.
        return Err(
            "zirv ctx nudge: mail is disabled (mail.enabled = false); nothing was sent".into(),
        );
    }

    let state = StateDir::resolve(env)?;
    // Reject short prefixes before resolution; uniqueness alone does not
    // prove operator intent for an action.
    if let Some(refusal) = prefix_too_short("nudge", &args.prefix, &live_shorts(&state)) {
        return Err(format!("zirv ctx nudge: {refusal}").into());
    }
    // Only live registry records are valid nudge targets.
    let addressed = resolve_prefix_or_parked(&state, &args.prefix).map_err(|e| {
        format!(
            "zirv ctx nudge: {}",
            resolve_error_with_diagnostics(&e, &state, env)
        )
    })?;
    // A parked seat has no live socket; deliver mail through its seat
    // recovery path, outside live-supervisor nudge rules. (#721)
    let record = match addressed {
        Addressed::Live(record) => record,
        Addressed::Parked(seat) => {
            let body = resolve_nudge_message(args, stdin)?;
            if body.is_empty() {
                return Err(
                    "zirv ctx nudge: no message given; pass --message, --message-file, or pipe one on stdin"
                        .into(),
                );
            }
            let super::seat::Phase::Parked {
                until,
                window,
                reason,
                ..
            } = seat.phase.clone()
            else {
                unreachable!("resolve_prefix_or_parked only returns Phase::Parked seats");
            };
            let now = super::state::now_secs();
            if until <= now {
                // Resume the seat directly; rollover could prepare a new
                // transaction with no live owner to unwind it. (#721)
                let _ = super::seat::resume(&state, &seat.short, now);
                super::rollover::record(
                    &state,
                    &seat.session,
                    "nudge",
                    super::rollover::RESUMED,
                    &super::rollover::PoolEvent {
                        snapshot_at: now,
                        binding_window: Some(window),
                        source_agent: seat.agent.clone(),
                        reason: "parked window elapsed".to_string(),
                        ..Default::default()
                    },
                );
            } else {
                writeln!(
                    w,
                    "zirv ctx nudge: session {} is parked until {} ({}); mail queued",
                    seat.short, until, reason
                )?;
            }
            let from_session = super::mail::identity_or_unknown(env, super::adapters::SESSION_ENV);
            let msg = super::mail::Message {
                from_session: from_session.clone(),
                from_agent: super::mail::identity_or_unknown(env, super::adapters::AGENT_ENV),
                to: seat.agent.clone(),
                to_session: Some(seat.short.clone()),
                sent: now,
                body,
            };
            // Seat state lacks a repo slug, so use the sender's repo for
            // this fallback delivery. (#721)
            let own_slug = super::state::repo_slug(repo);
            super::mail::store_to(&state, &own_slug, &own_slug, &msg, &cfg)?;
            write_nudge_marker(&state, &seat.short, &short_id(&from_session));
            writeln!(
                w,
                "zirv ctx nudge: queued for {} ({}) in {}",
                seat.short, seat.agent, own_slug
            )?;
            return Ok(0);
        }
    };

    // Refuse nudges for supervisors without a turn-signal socket; no one
    // can claim or announce their wake-up marker.
    if !record.reachable {
        return Err(format!(
            "zirv ctx nudge: session {} ({}) is not reachable for nudges -- it is running \
             without a turn-signal socket (`--no-supervise`, or the socket failed to bind), \
             so it never checks for wake-ups and would not even show an advisory. \
             Use `zirv ctx send --to-session {}` to leave a message for its next run.",
            record.short, record.verb, record.short
        )
        .into());
    }

    let body = resolve_nudge_message(args, stdin)?;
    if body.is_empty() {
        return Err(
            "zirv ctx nudge: no message given; pass --message, --message-file, or pipe one on stdin"
                .into(),
        );
    }

    let from_session = super::mail::identity_or_unknown(env, super::adapters::SESSION_ENV);
    let msg = super::mail::Message {
        from_session: from_session.clone(),
        from_agent: super::mail::identity_or_unknown(env, super::adapters::AGENT_ENV),
        to: record.agent.clone(),
        to_session: Some(record.short.clone()),
        sent: super::state::now_secs(),
        body,
    };
    // Deliver into the target's repository with its mail limits, even
    // when the sender is in another checkout.
    super::mail::store_to(
        &state,
        &record.repo_slug,
        &super::state::repo_slug(repo),
        &msg,
        &cfg,
    )?;
    // Store mail before marker; losing the marker can only delay delivery.
    // Carry sender identity so the target can name who woke it.
    write_nudge_marker(&state, &record.short, &short_id(&from_session));

    writeln!(
        w,
        "zirv ctx nudge: queued for {} ({}, {}) in {}",
        record.short, record.agent, record.verb, record.repo_slug
    )?;
    // Interactive nudges only advise; message bodies remain in inbox.
    if matches!(record.verb, Verb::Wrap | Verb::Chat) {
        writeln!(
            w,
            "zirv ctx nudge: {} is an interactive session; the guidance is delivered as \
             inbox mail plus a one-line advisory (typed in only at a verified-idle \
             boundary), never the message body itself",
            record.short
        )?;
    }
    Ok(0)
}

pub fn run_nudge<W: Write>(args: &NudgeArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_nudge_with(args, w, &repo, &env, &mut std::io::stdin())
}

// Kill uses OS process signals and does not depend on the target reading mail. (#166)

/// How long `kill` waits after SIGTERM before escalating to SIGKILL --
/// `supervise::terminate`'s own grace window for an owned `Child`.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, clap::Args)]
pub struct KillArgs {
    /// Short id (or a unique prefix of one, at least four characters) of the
    /// session to terminate. On unix a target whose pid the OS has since
    /// recycled to an unrelated process is deregistered without being
    /// signalled; where the check cannot run (Windows, or no `ps` on PATH)
    /// the registered pid is signalled as-is.
    pub prefix: String,
}

/// Allow small start-time slack when detecting a pid younger than its
/// registration; this one-sided check differs from record liveness tolerance. (#152)
const RECYCLED_PID_TOLERANCE_SECS: u64 = 5;

/// Refuse to signal a pid whose process began after registration; it may
/// belong to an unrelated process.
fn pid_looks_recycled(registered_at: u64, age_secs: u64, now: u64) -> bool {
    now.saturating_sub(age_secs) > registered_at.saturating_add(RECYCLED_PID_TOLERANCE_SECS)
}

/// Signal a live registered process and deregister only after confirmed
/// death; ask the owning dashboard first for panes so it can release its
/// writer permit. Check process start time before signaling when available.
/// On platforms without that probe, the registered pid remains the target. (#403)
pub fn run_kill_with<W: Write>(args: &KillArgs, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    if let Some(refusal) = prefix_too_short("kill", &args.prefix, &live_shorts(&state)) {
        return Err(format!("zirv ctx kill: {refusal}").into());
    }
    let record = resolve_prefix(&state, &args.prefix).map_err(|e| {
        format!(
            "zirv ctx kill: {}",
            resolve_error_with_diagnostics(&e, &state, env)
        )
    })?;

    // A recycled pid no longer belongs to this session: deregister only.
    if process_age_secs(record.pid)
        .is_some_and(|age| pid_looks_recycled(record.started_at, age, super::state::now_secs()))
    {
        let _ = std::fs::remove_file(record_path(&state, &record.short));
        writeln!(
            w,
            "zirv ctx kill: {} ({}, {}) is gone -- pid {} now belongs to a process that started \
             after the session registered, so nothing was signalled; deregistered it from the \
             session registry",
            record.short, record.agent, record.verb, record.pid
        )?;
        return Ok(0);
    }

    // Ask the pane's owning dashboard to stop it so reap releases its
    // writer permit promptly. (#403)
    if record.verb == Verb::Dash
        && let Some(ack) = kill_via_dashboard(&state, &record, env)
    {
        if ack.ok {
            writeln!(
                w,
                "zirv ctx kill: stopped {} ({}, {}, pid {}) through the dashboard that owns it, \
                 which released its writer permit and deregistered it",
                record.short, record.agent, record.verb, record.pid
            )?;
            return Ok(0);
        }
        writeln!(
            w,
            "zirv ctx kill: the dashboard owning {} would not stop it ({}); signalling pid {} \
             directly instead",
            record.short,
            ack.reason.as_deref().unwrap_or("no reason given"),
            record.pid
        )?;
    }

    report_kill_outcome(
        &state,
        &record,
        super::supervise::terminate_pid(record.pid, KILL_GRACE),
        w,
    )
}

/// Ask only the pane's live owning dashboard to stop it through this
/// requester's pane channel, never its shared channel: a request file
/// is data, not authority. Verify the channel owner pid and start time
/// before writing; a stale path must not be recreated. (#435)
/// Timeout falls back to direct signaling after withdrawing the request. (#179)
fn kill_via_dashboard(
    state: &StateDir,
    record: &Record,
    env: EnvLookup<'_>,
) -> Option<super::dash::spawnreq::SpawnAck> {
    use super::dash::spawnreq;

    let owner = record.owner_pid?;
    // A live dashboard must own this specific pane.
    super::dash::discover_live_dash_dirs(state)
        .into_iter()
        .find(|candidate| {
            matches!(candidate.status, super::dash::CandidateStatus::Live { pid, .. } if pid == owner)
        })?;
    let dir = non_empty(env(spawnreq::DASH_REQUESTS_ENV))?;
    let dir = Path::new(&dir);
    let owner_pid_path = spawnreq::owner_pid_path(dir);
    let channel_owner = std::fs::read_to_string(&owner_pid_path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok());
    if channel_owner != Some(owner) {
        return None;
    }
    let channel_registered_at = std::fs::metadata(&owner_pid_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    if channel_registered_at
        .zip(process_age_secs(owner))
        .is_some_and(|(registered_at, age)| {
            pid_looks_recycled(registered_at, age, super::state::now_secs())
        })
    {
        return None;
    }

    let req = spawnreq::SpawnRequest {
        kill: Some(record.short.clone()),
        requested_by: "ctx kill".to_string(),
        ..Default::default()
    };
    let path = spawnreq::write_request(dir, &req).ok()?;
    let stem = spawnreq::request_stem(&path)?;
    let ack = spawnreq::wait_for_ack(dir, &stem, super::agent::DASH_ACK_TIMEOUT);
    if ack.is_none() {
        // Withdraw a timed-out request so late processing cannot kill a
        // pane after this call reports failure.
        let _ = std::fs::remove_file(&path);
    }
    ack
}

/// Report kill outcome and deregister only after confirmed process death;
/// refused signals leave the live session visible. (#403)
fn report_kill_outcome<W: Write>(
    state: &StateDir,
    record: &Record,
    outcome: super::supervise::KillOutcome,
    w: &mut W,
) -> CtxResult<i32> {
    match outcome {
        super::supervise::KillOutcome::Terminated => {
            let _ = std::fs::remove_file(record_path(state, &record.short));
            writeln!(
                w,
                "zirv ctx kill: terminated {} ({}, {}, pid {}) and deregistered it",
                record.short, record.agent, record.verb, record.pid
            )?;
            Ok(0)
        }
        super::supervise::KillOutcome::Refused { errno, signal } => {
            writeln!(
                w,
                "zirv ctx kill: could not signal {} ({}, {}, pid {}): {signal} refused (errno \
                 {errno}); nothing was sent and the session stays registered -- run `kill -TERM \
                 {}` from an unsandboxed shell, or kill the pane from the dashboard",
                record.short, record.agent, record.verb, record.pid, record.pid
            )?;
            Ok(1)
        }
        super::supervise::KillOutcome::Survived => {
            writeln!(
                w,
                "zirv ctx kill: {} ({}, {}, pid {}) survived SIGTERM and SIGKILL after {}s; the \
                 session stays registered",
                record.short,
                record.agent,
                record.verb,
                record.pid,
                KILL_GRACE.as_secs()
            )?;
            Ok(1)
        }
    }
}

pub fn run_kill<W: Write>(args: &KillArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    run_kill_with(args, w, &env)
}

#[cfg(test)]
mod tests {
    use super::super::testenv::dead_pid;
    use super::super::tests::{env_map, record_for, sh, state_in};
    use super::*;

    #[test]
    fn a_nudge_stores_a_session_addressed_message_and_a_wake_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Exec);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: "abcd".to_string(),
            message: Some("please check the new failing test".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("nudge");
        assert_eq!(code, 0);

        let slug = super::super::state::repo_slug(&repo);
        let listed = super::super::mail::list(&state, &slug, None, Some(&short)).expect("list");
        assert_eq!(listed.len(), 1, "the payload is durable, ordinary mail");
        assert_eq!(listed[0].1.to_session, Some(short.clone()));
        assert_eq!(listed[0].1.body, "please check the new failing test");

        assert!(
            nudge_marker_path(&state, &short).is_file(),
            "the wake-up marker exists alongside the payload"
        );
    }

    /// Issue #721 review finding #3: `run_nudge_with` has no end-to-end
    /// coverage of the ghost-park paths `resolve_prefix_or_parked` added --
    /// mirrors `mail::tests::
    /// send_to_a_ghost_parked_seat_past_its_window_resumes_before_delivering`.
    /// A due ghost park resumes to `Phase::Idle` before the nudge's payload
    /// is delivered -- in place, even when another harness is clearly the
    /// better fit (the same exhausted-vs-open usage recipe as `mail::tests::
    /// send_to_a_due_ghost_parked_seat_never_opens_a_handover_even_when_another_harness_is_better`),
    /// so a regression back to `rollover::on_resume` fails here too.
    #[test]
    fn nudge_to_a_due_ghost_parked_seat_resumes_before_delivering() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join("ctx.toml"),
            format!(
                "agent_bin = {:?}\n\
                 [pace]\nestimator = false\n\
                 [fallback]\nauto_orchestrator_rollover = true\n\
                 orchestrator_rollover_headroom_pct = 20.0\n\
                 min_candidate_headroom_pct = 10.0\n",
                std::env::current_exe()
                    .expect("current test executable")
                    .display()
                    .to_string()
            ),
        )
        .expect("write ctx.toml");
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let now = super::super::state::now_secs();
        crate::commands::ctx::seat::register(
            &state,
            "duenudg1",
            "due-nudge-session",
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            now,
        )
        .expect("register");
        for (provider, used_percentage) in [("anthropic", 100.0), ("openai", 5.0)] {
            crate::commands::ctx::window::store_for(
                &state,
                provider,
                &crate::commands::ctx::window::UsageWindows {
                    five_hour: Some(crate::commands::ctx::window::Window {
                        used_percentage,
                        resets_at: now + 3_600,
                        observed_at: now,
                        overage_covered: false,
                        limit_reached: false,
                    }),
                    seven_day: None,
                },
            )
            .expect("store usage");
        }
        // Already elapsed: `until` is in the past.
        crate::commands::ctx::seat::park(
            &state,
            "duenudg1",
            now.saturating_sub(60),
            "5h",
            "usage exhausted",
            now,
        )
        .expect("park");

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: "duenudg1".to_string(),
            message: Some("still there?".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("nudge");
        assert_eq!(code, 0);

        let seat = crate::commands::ctx::seat::load(&state, "duenudg1").expect("seat exists");
        assert!(
            matches!(seat.phase, crate::commands::ctx::seat::Phase::Idle),
            "a due ghost park must resume in place before delivery, never open a handover, \
             got {:?}",
            seat.phase
        );

        let slug = super::super::state::repo_slug(&repo);
        let listed = super::super::mail::list(&state, &slug, None, Some("duenudg1")).expect("list");
        assert_eq!(listed.len(), 1, "the nudge payload is still delivered");
        assert_eq!(listed[0].1.body, "still there?");
    }

    /// A ghost-parked seat whose window has NOT elapsed queues normally,
    /// reports the park instead of resuming early, and -- unlike the due
    /// case above -- leaves the seat file byte-identical: nothing about
    /// this nudge may touch the seat before its own window says so.
    #[test]
    fn nudge_to_a_ghost_parked_seat_before_its_window_queues_and_leaves_the_seat_file_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let now = super::super::state::now_secs();
        crate::commands::ctx::seat::register(
            &state,
            "notnudg1",
            "notdue-nudge-session",
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            now,
        )
        .expect("register");
        let until = now + 3600;
        crate::commands::ctx::seat::park(&state, "notnudg1", until, "5h", "usage exhausted", now)
            .expect("park");

        let seat_path = state.sessions().join("notnudg1.seat.json");
        let before = std::fs::read(&seat_path).expect("seat file exists");

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: "notnudg1".to_string(),
            message: Some("checking in".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("nudge");
        assert_eq!(code, 0);

        let after = std::fs::read(&seat_path).expect("seat file still exists");
        assert_eq!(
            before, after,
            "a not-yet-due ghost park must leave the seat file byte-identical"
        );

        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("is parked until") && printed.contains("usage exhausted"),
            "must surface the park instead of a bare not-found: {printed}"
        );

        let slug = super::super::state::repo_slug(&repo);
        let listed = super::super::mail::list(&state, &slug, None, Some("notnudg1")).expect("list");
        assert_eq!(listed.len(), 1, "the nudge payload is still queued");
        assert_eq!(listed[0].1.body, "checking in");
    }

    #[test]
    fn the_marker_is_claimed_exactly_once_even_with_two_observers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        write_nudge_marker(&state, "aaaa1111", "bbbb2222");

        assert_eq!(
            claim_nudge_marker(&state, "aaaa1111").as_deref(),
            Some("bbbb2222"),
            "the first observer claims it, and learns who sent it"
        );
        assert_eq!(
            claim_nudge_marker(&state, "aaaa1111"),
            None,
            "a second observer finds nothing left to claim"
        );
    }

    // -- stall marker (issue #310, 3a) ------------------------------------

    #[test]
    fn a_freshly_armed_stall_marker_reads_back_its_own_timestamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        write_stall_marker(&state, "aaaa1111", 1_000);
        assert_eq!(stall_marker(&state, "aaaa1111"), Some(1_000));
    }

    #[test]
    fn a_stall_marker_is_absent_until_written() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        assert_eq!(stall_marker(&state, "aaaa1111"), None);
    }

    /// Unlike the nudge marker, reading the stall marker must not consume
    /// it -- both the owning poll loop and a dashboard render observe the
    /// same latch repeatedly.
    #[test]
    fn reading_a_stall_marker_does_not_consume_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        write_stall_marker(&state, "aaaa1111", 1_000);
        assert_eq!(stall_marker(&state, "aaaa1111"), Some(1_000));
        assert_eq!(
            stall_marker(&state, "aaaa1111"),
            Some(1_000),
            "a second read must still see it"
        );
    }

    #[test]
    fn clearing_a_stall_marker_removes_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        write_stall_marker(&state, "aaaa1111", 1_000);
        clear_stall_marker(&state, "aaaa1111");
        assert_eq!(stall_marker(&state, "aaaa1111"), None);
    }

    #[test]
    fn clearing_a_stall_marker_that_was_never_armed_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        clear_stall_marker(&state, "aaaa1111");
        assert_eq!(stall_marker(&state, "aaaa1111"), None);
    }

    #[test]
    fn a_malformed_stall_marker_reads_as_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        std::fs::write(stall_marker_path(&state, "aaaa1111"), b"not-a-number").expect("write");
        assert_eq!(stall_marker(&state, "aaaa1111"), None);
    }

    /// C4: the marker carries the *sender's* short id. Every emitter used to
    /// pass its own id into `Event::Nudge`, so the announcement always read
    /// "nudged by <myself>".
    #[test]
    fn claiming_a_marker_reports_who_sent_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());

        write_nudge_marker(&state, "target01", "sender99");
        assert_eq!(
            claim_nudge_marker(&state, "target01").as_deref(),
            Some("sender99")
        );

        // An empty marker (a pre-C4 writer, or a truncated write) still
        // claims -- the wake-up matters more than the attribution.
        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        std::fs::write(nudge_marker_path(&state, "target01"), b"").expect("write");
        assert_eq!(
            claim_nudge_marker(&state, "target01").as_deref(),
            Some(UNKNOWN_SENDER)
        );

        // Whitespace is not a sender either.
        std::fs::write(
            nudge_marker_path(&state, "target01"),
            b"  
",
        )
        .expect("write");
        assert_eq!(
            claim_nudge_marker(&state, "target01").as_deref(),
            Some(UNKNOWN_SENDER)
        );
    }

    /// C8: a marker whose session is gone is swept with the record, on the
    /// same read. Left behind, it would be claimed by whichever supervisor
    /// next registers under that short id -- much likelier now that a short
    /// id is a stable address rather than a per-cycle value.
    #[test]
    fn orphaned_markers_are_swept_with_their_records() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        // One live session with an unclaimed marker, one dead session with
        // one, and one marker whose record never existed at all.
        let live = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let live_short = live.short.clone();
        write_record(&state, &live);
        write_nudge_marker(&state, &live_short, "sender01");

        let mut dead = record_for("22222222-2222-4333-8444-555555555555", &repo, Verb::Exec);
        dead.pid = dead_pid();
        let dead_short = dead.short.clone();
        write_record(&state, &dead);
        write_nudge_marker(&state, &dead_short, "sender02");

        write_nudge_marker(&state, "99999999", "sender03");

        let _ = list(&state);

        assert!(
            nudge_marker_path(&state, &live_short).is_file(),
            "a live session's unclaimed marker is left alone"
        );
        assert!(
            !nudge_marker_path(&state, &dead_short).exists(),
            "a dead session's marker is swept with its record"
        );
        assert!(
            !nudge_marker_path(&state, "99999999").exists(),
            "a marker with no record at all is swept too"
        );
    }

    /// Both markers are read by OTHER processes while their owner rewrites
    /// them -- `claim_nudge_marker` could hand back an empty sender, and
    /// `stall_marker` could lose the latch for a tick -- so neither may be
    /// written with a truncating write. Every other writer of these files
    /// already goes through `state::write_private`.
    #[test]
    fn marker_writes_are_atomic_for_a_concurrent_reader() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let short = "abcd1234";
        write_nudge_marker(&state, short, "sender01");
        write_stall_marker(&state, short, 1_700_000_000);

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let nudge = nudge_marker_path(&state, short);
            let stall = stall_marker_path(&state, short);
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut torn = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(text) = std::fs::read_to_string(&nudge)
                        && text.trim().is_empty()
                    {
                        torn += 1;
                    }
                    if let Ok(text) = std::fs::read_to_string(&stall)
                        && text.trim().parse::<u64>().is_err()
                    {
                        torn += 1;
                    }
                }
                torn
            })
        };

        for i in 0..600u64 {
            write_nudge_marker(&state, short, "sender01");
            write_stall_marker(&state, short, 1_700_000_000 + i);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let torn = reader.join().expect("reader thread");

        assert_eq!(
            torn, 0,
            "a concurrent reader saw a marker mid-write {torn} time(s)"
        );
        assert_eq!(
            claim_nudge_marker(&state, short).as_deref(),
            Some("sender01")
        );
        assert_eq!(stall_marker(&state, short), Some(1_700_000_599));
    }

    // F6: a unique-but-mistyped prefix must not be actionable.

    #[test]
    fn a_nudge_prefix_shorter_than_four_characters_is_refused_with_the_session_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Chat);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        // "abc" is *unique* here -- exactly the shape that used to resolve and
        // nudge the wrong session on a typo.
        let args = NudgeArgs {
            prefix: "abc".to_string(),
            message: Some("wake up".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("three characters is not enough to nudge on");
        let msg = err.to_string();
        assert!(msg.contains("prefix too short"), "got {msg}");
        assert!(msg.contains(&short), "names what could be typed: {msg}");

        // Nothing was queued and nothing was woken.
        let slug = super::super::state::repo_slug(&repo);
        assert!(
            super::super::mail::list(&state, &slug, None, Some(&short))
                .expect("list")
                .is_empty(),
            "a refused nudge must not store a message"
        );
        assert!(!nudge_marker_path(&state, &short).exists());

        // Four characters, and the whole short id, both still work.
        let args = NudgeArgs {
            prefix: "abcd".to_string(),
            message: Some("wake up".to_string()),
            message_file: None,
        };
        let code = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("four characters is enough");
        assert_eq!(code, 0);
    }

    #[test]
    fn the_minimum_prefix_rule_still_admits_a_whole_short_id() {
        // Pure rule, no registry: a session whose entire short id is shorter
        // than the minimum is still addressable by that whole id, and only by
        // it.
        let shorts = vec!["ab".to_string()];
        assert_eq!(prefix_too_short("nudge", "ab", &shorts), None);
        assert!(prefix_too_short("nudge", "a", &shorts).is_some());
        assert_eq!(prefix_too_short("nudge", "abcd", &[]), None);
        let refusal = prefix_too_short("nudge", "x", &[]).expect("refused");
        assert!(refusal.contains("none registered"), "got {refusal}");
    }

    /// Issue #146: the same diagnostic extension as `mail::run_send_with`'s
    /// own regression test -- a prefix long enough to pass the minimum-length
    /// guard but matching no live session must name the state dir it was
    /// actually checked against, not just say "no sessions are registered".
    #[test]
    fn an_unresolvable_nudge_prefix_names_the_state_dir_it_was_checked_against() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);

        let args = NudgeArgs {
            prefix: "dead0000".to_string(),
            message: Some("wake up".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("no session is registered under this empty state dir");

        let msg = err.to_string();
        assert!(
            msg.contains("no sessions are registered"),
            "the existing message text stays the prefix: {msg}"
        );
        let state = state_in(&state_dir);
        assert!(
            msg.contains(&state.sessions().display().to_string()),
            "must name the registry path actually checked: {msg}"
        );
        assert!(
            msg.contains(super::super::state::STATE_ENV),
            "must say whether ZIRV_CTX_STATE_DIR was set: {msg}"
        );
    }

    // NEW-2: the delivery address each supervisor lists its own mail under.
    // Reverting a seam to `None` (loop's old behavior) or to
    // `short_id(current session)` (exec's old behavior) has to break
    // something, not just quietly change routing.

    #[test]
    fn the_delivery_filter_is_never_an_unfiltered_listing() {
        // `None` means "no session filter at all", which makes a supervisor
        // read *and consume* other sessions' directed mail.
        assert!(delivery_filter(Some("aaaa1111"), "bbbb2222").is_some());
        assert!(delivery_filter(None, "bbbb2222").is_some());
    }

    #[test]
    fn the_loop_delivery_address_is_the_registry_short() {
        // Before the first cycle registers there is nothing latched, so the
        // address is the short about to be registered.
        assert_eq!(delivery_filter(None, "cycle001"), Some("cycle001"));

        // From then on the latched registry short wins over whatever short
        // the current cycle happens to have minted.
        assert_eq!(
            delivery_filter(Some("cycle001"), "cycle002"),
            Some("cycle001"),
            "a loop must keep answering to the address a sender resolved,              not to this cycle's own fresh id"
        );
        assert_eq!(
            delivery_filter(Some("cycle001"), "cycle009"),
            Some("cycle001"),
            "and must keep doing so however many cycles later"
        );
    }

    /// The exec seam, expressed against a real guard: the address a sender
    /// resolved has to keep working after the session underneath it rotates.
    #[test]
    fn the_exec_delivery_address_survives_session_rotation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("aaaa1111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let launch_short = record.short.clone();
        let mut guard = SessionGuard::register(&state, record);

        assert_eq!(
            delivery_filter(Some(guard.short()), &launch_short),
            Some(launch_short.as_str()),
            "at launch the guard's address and the launch short agree"
        );

        // A restart mints a fresh session id...
        let restarted = "bbbb2222-2222-4333-8444-555555555555";
        guard.refresh_session(restarted);
        let rotated_short = short_id(restarted);
        assert_ne!(
            rotated_short, launch_short,
            "sanity: the session's own short really did change"
        );

        // ...and the delivery address does not follow it.
        assert_eq!(
            delivery_filter(Some(guard.short()), &rotated_short),
            Some(launch_short.as_str()),
            "mail addressed before the restart must still reach this run"
        );
    }

    /// C1: `resolve_prefix` searches the machine-wide registry, so a nudge
    /// can land on a session running in a different checkout. Storing its
    /// payload under the *sender's* repo slug filed it in a mailbox that
    /// session never reads -- the wake-up fired, the message never arrived.
    #[test]
    fn a_cross_repo_nudge_delivers_into_the_targets_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);

        let sender_repo = tmp.path().join("sender-repo");
        let target_repo = tmp.path().join("target-repo");
        let record = record_for(
            "abcdef12-3456-4789-8abc-def012345678",
            &target_repo,
            Verb::Exec,
        );
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: short.clone(),
            message: Some("please check the new failing test".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let code = run_nudge_with(
            &args,
            &mut out,
            &sender_repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge");
        assert_eq!(code, 0);

        let delivered = super::super::mail::list(
            &state,
            &super::super::state::repo_slug(&target_repo),
            None,
            Some(&short),
        )
        .expect("list");
        assert_eq!(
            delivered.len(),
            1,
            "the payload must land in the repo the target session actually reads"
        );
        assert_eq!(delivered[0].1.body, "please check the new failing test");

        assert!(
            super::super::mail::list(
                &state,
                &super::super::state::repo_slug(&sender_repo),
                None,
                None
            )
            .expect("list")
            .is_empty(),
            "nothing is filed under the sender's own repo"
        );

        // C1: the confirmation names the resolved target and where it went.
        let printed = String::from_utf8(out).expect("utf8");
        assert!(printed.contains(&short), "names the session: {printed}");
        assert!(
            printed.contains(&super::super::state::repo_slug(&target_repo)),
            "names the repo it was delivered into: {printed}"
        );
    }

    /// N6: an interactive target is never restarted and never typed into, and
    /// never receives message bodies. Saying so is the difference between
    /// "the nudge is broken" and "a human has to go read it".
    #[test]
    fn nudging_an_interactive_session_says_it_is_advisory_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);

        for verb in [Verb::Chat, Verb::Wrap] {
            let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, verb);
            let short = record.short.clone();
            let mut guard = SessionGuard::register(&state, record);

            let args = NudgeArgs {
                prefix: short.clone(),
                message: Some("look at this".to_string()),
                message_file: None,
            };
            let mut out = Vec::new();
            let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
            run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
                .expect("nudge");
            let printed = String::from_utf8(out).expect("utf8");
            assert!(
                printed.contains("interactive session"),
                "{verb} must be called out as interactive: {printed}"
            );
            assert!(
                printed.contains("inbox"),
                "and must say where the guidance actually shows up: {printed}"
            );
            guard.release();
        }

        // A headless target says nothing of the sort -- it really does act
        // on the guidance.
        let record = record_for("bbbbbbbb-3456-4789-8abc-def012345678", &repo, Verb::Exec);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);
        let args = NudgeArgs {
            prefix: short,
            message: Some("look at this".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect("nudge");
        assert!(
            !String::from_utf8(out)
                .expect("utf8")
                .contains("interactive"),
            "a headless worker is not advisory-only"
        );
    }

    // NEW-3: a supervisor with no turn-signal socket stays *visible* but is
    // refused as a nudge target, rather than being hidden from the registry
    // (which cured the silent nudge by making the session invisible).

    #[test]
    fn an_unsupervised_wrap_is_not_a_nudge_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let record =
            record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Wrap).unreachable();
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        // Still listed -- an operator must be able to see it running.
        let listed = list(&state);
        assert_eq!(listed.len(), 1, "an unreachable session is not hidden");
        assert!(!listed[0].0.reachable);
        assert_eq!(listed[0].1, Liveness::Live);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: short.clone(),
            message: Some("please look at this".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("a session with no signal socket cannot act on a nudge");
        let msg = err.to_string();
        assert!(msg.contains("not reachable"), "got {msg}");
        assert!(
            msg.contains("turn-signal socket"),
            "must say why, not just that: {msg}"
        );
        assert!(
            msg.contains("zirv ctx send"),
            "must offer the thing that does work: {msg}"
        );

        // Nothing was queued and no marker was left behind to be claimed by
        // whatever registers under this address next.
        let slug = super::super::state::repo_slug(&repo);
        assert!(
            super::super::mail::list(&state, &slug, None, Some(&short))
                .expect("list")
                .is_empty()
        );
        assert!(!nudge_marker_path(&state, &short).exists());
    }

    #[test]
    fn nudging_an_unknown_or_dead_session_is_an_error_that_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let repo = tmp.path().join("repo");

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = NudgeArgs {
            prefix: "zzzz".to_string(),
            message: Some("hello?".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("no session is registered at all");
        assert!(err.to_string().contains("no session"), "got {err}");

        // A dead session (its process gone) is swept from the registry on
        // read, so it surfaces exactly the same way as unknown -- there is
        // nothing left to disambiguate it from "never existed".
        let state = state_in(&state_dir);
        let mut dead = record_for("dddddddd-2222-4333-8444-555555555555", &repo, Verb::Exec);
        dead.pid = dead_pid();
        write_record(&state, &dead);

        let args = NudgeArgs {
            prefix: "dddd".to_string(),
            message: Some("hello?".to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        let err = run_nudge_with(&args, &mut out, &repo, &|k| env.get(k).cloned(), &mut stdin)
            .expect_err("the only match is dead");
        assert!(err.to_string().contains("no session"), "got {err}");
    }

    /// Issue #166: `zirv ctx kill` must work against a session that has no
    /// way to notice anything -- unlike `nudge`, this never depends on the
    /// target reading mail or waking itself. Ends the real process by pid and
    /// deregisters the record, both without a `SessionGuard` of its own: a
    /// separate `kill` invocation only ever has the registry record, never
    /// the original guard.
    #[test]
    fn kill_terminates_a_live_sessions_process_and_deregisters_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let mut child = sh("sleep 30")
            .spawn()
            .expect("spawn a real process to kill");
        let pid = child.id();
        // `run_kill_with`'s own liveness check is `is_alive` (`kill(pid,
        // 0)`), not `Child::try_wait` -- this test is the process's real
        // parent, so it must reap concurrently or the killed process would
        // sit as a zombie (still visible to `kill(pid, 0)`) until this test's
        // own `wait()` ran. See `supervise::terminate_pid`'s own tests for
        // the same reasoning.
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        let mut record = record_for("eeeeeeee-2222-4333-8444-555555555555", &repo, Verb::Exec);
        record.pid = pid;
        let short = record.short.clone();
        let path = record_path(&state, &short);
        write_record(&state, &record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = KillArgs {
            prefix: short.clone(),
        };
        let mut out = Vec::new();
        let code =
            run_kill_with(&args, &mut out, &|k| env.get(k).cloned()).expect("kill must succeed");
        assert_eq!(code, 0);
        reaper.join().expect("reaper thread");

        assert!(!path.exists(), "the record is deregistered");
        assert!(
            list(&state).is_empty(),
            "nothing left to resolve a future nudge or kill against"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&short), "names what it killed: {text}");
    }

    /// SECURITY (issue #435 item 1): `kill_via_dashboard` writes into THIS
    /// process's own pane channel (`DASH_REQUESTS_ENV`), never the owning
    /// dashboard's shared one -- the dashboard now refuses every `kill` that
    /// arrives on its shared channel outright, so writing there would just
    /// be a wasted round-trip. The fake dashboard here is only a live
    /// `owner.pid` naming this test's own pid (so `discover_live_dash_dirs`
    /// sees a live owner); nothing ever drains its shared `requests`
    /// directory, and this test asserts the request never lands there.
    #[test]
    fn kill_via_dashboard_writes_the_request_on_the_callers_own_channel_not_the_shared_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        let token_dir = state.dash().join("faketoken");
        std::fs::create_dir_all(&token_dir).expect("mkdir token dir");
        std::fs::write(token_dir.join("owner.pid"), std::process::id().to_string())
            .expect("write owner.pid");
        let shared_requests_dir = token_dir.join("requests");

        // A pane's own channel is a SIBLING of the shared `requests` leaf
        // under the same token dir (`spawnreq::pane_request_dir_for`), so
        // its `owner.pid` is the token dir's -- the new same-owner check
        // needs that real layout, not a channel floating free of any token
        // dir.
        let own_channel = token_dir.join("p-faketoken");
        let env = env_map(&[(
            super::super::dash::spawnreq::DASH_REQUESTS_ENV,
            own_channel.to_str().expect("utf8"),
        )]);

        let mut record = record_for("ffffffff-2222-4333-8444-555555555555", &repo, Verb::Dash);
        record.owner_pid = Some(std::process::id());
        let target_short = record.short.clone();

        let handle = std::thread::spawn({
            let state = state.clone();
            let record = record.clone();
            let env = env.clone();
            move || kill_via_dashboard(&state, &record, &|k| env.get(k).cloned())
        });

        // Poll the requester's own channel for the request `kill_via_
        // dashboard` should have written there, then answer it exactly as a
        // real dashboard's `drain_one_channel` would -- this test exercises
        // only where the client writes, not the dashboard side.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut found = None;
        while std::time::Instant::now() < deadline {
            let batch = super::super::dash::spawnreq::take_requests(&own_channel);
            if let Some(pair) = batch.into_iter().next() {
                found = Some(pair);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (path, req) = found.expect("the request arrives on the caller's own channel");
        assert_eq!(req.kill.as_deref(), Some(target_short.as_str()));
        let stem = super::super::dash::spawnreq::request_stem(&path).expect("stem");
        super::super::dash::spawnreq::write_ack(
            &own_channel,
            &stem,
            &super::super::dash::spawnreq::SpawnAck {
                ok: true,
                short: Some(target_short.clone()),
                reason: None,
                retryable: false,
                budget_exhausted: false,
                capability_warnings: Vec::new(),
            },
        )
        .expect("write ack");

        let ack = handle
            .join()
            .expect("thread")
            .expect("an ack came back over the caller's own channel");
        assert!(ack.ok);
        assert_eq!(ack.short.as_deref(), Some(target_short.as_str()));
        assert!(
            !shared_requests_dir.exists(),
            "the owning dashboard's own shared channel never received anything"
        );
    }

    /// The pure half of the recycled-pid guard: only a process that started
    /// AFTER the session that claims its pid is a stranger. A process older
    /// than its own record is the ordinary case (a supervisor registers
    /// itself moments after it starts, and a dashboard registers a pane's
    /// record long after the dashboard process itself began), and must never
    /// be mistaken for a recycled one.
    #[test]
    fn pid_looks_recycled_only_flags_a_process_younger_than_its_own_record() {
        let now = 1_700_000_000;
        assert!(
            !pid_looks_recycled(now - 60, 3_600, now),
            "a process much older than its record is the ordinary case"
        );
        assert!(
            !pid_looks_recycled(now, 0, now),
            "and one that started in the same instant is not a stranger either"
        );
        assert!(
            !pid_looks_recycled(now - RECYCLED_PID_TOLERANCE_SECS, 0, now),
            "the tolerance absorbs a coarse reading and a stepped clock"
        );
        assert!(
            pid_looks_recycled(now - 3_600, 5, now),
            "a process seconds old under an hour-old record is a recycled pid"
        );
    }

    /// The whole point of the guard, end to end on a real process: `kill`
    /// must not SIGTERM a stranger that merely inherited a dead session's
    /// pid. Simulated the only way a test can without waiting for the OS to
    /// wrap its pid counter -- a genuinely fresh process, under a record
    /// that claims to predate it by an hour.
    #[cfg(unix)]
    #[test]
    fn kill_deregisters_a_recycled_pid_without_signalling_it() {
        if process_age_secs(std::process::id()).is_none() {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to check");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let mut child = sh("sleep 30").spawn().expect("spawn a stand-in process");
        let pid = child.id();

        let mut record = record_for("cccccccc-2222-4333-8444-555555555555", &repo, Verb::Exec);
        record.pid = pid;
        record.started_at = super::super::state::now_secs() - 3_600;
        let short = record.short.clone();
        let path = record_path(&state, &short);
        write_record(&state, &record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = KillArgs {
            prefix: short.clone(),
        };
        let mut out = Vec::new();
        let code = run_kill_with(&args, &mut out, &|k| env.get(k).cloned()).expect("kill runs");

        assert_eq!(code, 0);
        assert!(!path.exists(), "the dead session is still deregistered");
        assert!(
            is_alive(pid),
            "but the unrelated process holding its pid must be left alone"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("nothing was signalled"),
            "and the operator is told why: {text}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Issue #403: the three genuinely different endings `supervise::
    /// terminate_pid` can reach, and what each one is allowed to say and to
    /// do to the registry. Driven through the mapping rather than through a
    /// real signal -- an `EPERM` refusal is not something a test can provoke
    /// from the process that owns the target -- because the bug was never in
    /// the syscall: it was in reporting a refused signal as a sent one and
    /// deregistering a session that was still running and still holding its
    /// writer permit.
    #[test]
    fn only_a_confirmed_death_deregisters_the_record() {
        use crate::commands::ctx::supervise::KillOutcome;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(&tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let mut record = record_for("abababab-2222-4333-8444-555555555555", &repo, Verb::Dash);
        record.pid = 4242;
        let path = record_path(&state, &record.short);
        write_record(&state, &record);

        let mut out = Vec::new();
        let code = report_kill_outcome(
            &state,
            &record,
            KillOutcome::Refused {
                errno: 1,
                signal: "SIGTERM",
            },
            &mut out,
        )
        .expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, 1, "a refused signal is not a success: {text}");
        assert!(
            text.contains("SIGTERM refused (errno 1)"),
            "names the signal and the errno: {text}"
        );
        assert!(
            text.contains("nothing was sent and the session stays registered"),
            "and never claims to have sent one: {text}"
        );
        assert!(
            text.contains("kill -TERM 4242"),
            "and says what to run instead: {text}"
        );
        assert!(
            path.exists(),
            "a session whose process is still running stays in the registry"
        );

        let mut out = Vec::new();
        let code =
            report_kill_outcome(&state, &record, KillOutcome::Survived, &mut out).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(
            code, 1,
            "nor is a process that outlived both signals: {text}"
        );
        assert!(
            text.contains("survived SIGTERM and SIGKILL after 5s"),
            "{text}"
        );
        assert!(text.contains("the session stays registered"), "{text}");
        assert!(path.exists(), "so its record stays too");

        let mut out = Vec::new();
        let code = report_kill_outcome(&state, &record, KillOutcome::Terminated, &mut out)
            .expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, 0);
        assert!(
            text.contains("terminated") && text.contains("deregistered it"),
            "{text}"
        );
        assert!(!path.exists(), "only a confirmed death deregisters");
    }

    /// Issue #403: the recycling guard is NOT what removed the live session
    /// behind the reported kill -- a pane's record is written the instant it
    /// spawns, so its process always predates it and both halves of the guard
    /// must agree it is genuine. Pinned against a real live child so a future
    /// tightening of either half cannot start discarding live panes silently.
    #[cfg(unix)]
    #[test]
    fn a_freshly_registered_live_child_never_looks_like_a_recycled_pid() {
        let mut child = sh("sleep 30").spawn().expect("spawn a stand-in pane");
        let pid = child.id();
        let registered_at = super::super::state::now_secs();
        match process_age_secs(pid) {
            Some(age) => assert!(
                !pid_looks_recycled(registered_at, age, super::super::state::now_secs()),
                "a child that predates its own record must never look recycled (age {age}s)"
            ),
            None => eprintln!("skipping: no usable `ps` in this environment"),
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// `resolve_prefix`'s own contract, reused verbatim by `kill`: an unknown
    /// prefix and a prefix whose only match already died (and was therefore
    /// already swept and deregistered by `list`) surface identically -- there
    /// is nothing left to kill or deregister either way.
    #[test]
    fn killing_an_unknown_or_dead_session_is_an_error_that_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let repo = tmp.path().join("repo");
        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);

        let args = KillArgs {
            prefix: "zzzz".to_string(),
        };
        let mut out = Vec::new();
        let err = run_kill_with(&args, &mut out, &|k| env.get(k).cloned())
            .expect_err("no session is registered at all");
        assert!(err.to_string().contains("no session"), "got {err}");

        let state = state_in(&state_dir);
        let mut dead = record_for("dddddddd-2222-4333-8444-555555555555", &repo, Verb::Loop);
        dead.pid = dead_pid();
        write_record(&state, &dead);

        let args = KillArgs {
            prefix: "dddd".to_string(),
        };
        let mut out = Vec::new();
        let err = run_kill_with(&args, &mut out, &|k| env.get(k).cloned())
            .expect_err("the only match is already dead");
        assert!(err.to_string().contains("no session"), "got {err}");
    }

    /// Same guard `nudge` already has, on the same shared rule: a kill is at
    /// least as destructive as a nudge (it ends the process outright), so a
    /// mistyped one- or two-character prefix must not resolve just because it
    /// happens to be unique on this machine.
    #[test]
    fn a_kill_prefix_shorter_than_four_characters_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");
        let record = record_for("abcdef12-3456-4789-8abc-def012345678", &repo, Verb::Chat);
        let _guard = SessionGuard::register(&state, record);

        let env = env_map(&[(
            super::super::state::STATE_ENV,
            state_dir.to_str().expect("utf8"),
        )]);
        let args = KillArgs {
            prefix: "abc".to_string(),
        };
        let mut out = Vec::new();
        let err = run_kill_with(&args, &mut out, &|k| env.get(k).cloned())
            .expect_err("three characters is not enough to kill on");
        assert!(err.to_string().contains("prefix too short"), "got {err}");
    }
}
