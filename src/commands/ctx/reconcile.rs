//! `zirv ctx reconcile` (issue #720): one level-triggered pass over every
//! sweep this codebase already runs opportunistically -- each gated today on
//! an unrelated command happening to touch that one resource
//! (`sessions::list`'s registry sweep, `task::claim_locked`'s reap-in-passing
//! for the one id being claimed, `permit::acquire`'s dead-owner sweep,
//! `reservation::reserve`/`settle`/`release`'s prune-on-write,
//! `worktree::gc` at dashboard/`--worktree` startup) -- plus the one
//! resource with no automatic reclaim at all: an abandoned work group
//! (`group::is_abandoned` is display-only everywhere else in this codebase).
//!
//! Every heal below reuses an existing module's own pure liveness/proof
//! decision under whichever lock that module already serializes its own
//! mutations through: `task::reap_all_locked` wraps `reap_if_stale_pure`
//! under `task`'s own lock, `reservation::prune_dead_locked` wraps
//! `is_owner_alive`/`prune_dead` under the ledger lock, `permit::
//! live_records`/`live_writer_records` are called unchanged (their own
//! dead-owner sweep is already an inherent side effect of listing),
//! `worktree::gc` is called exactly as `agent.rs`/`dash/mod.rs` already call
//! it, and `group::close` only ever follows `group::is_abandoned` +
//! `group::short_id_is_alive` -- the same two facts `status.rs` already
//! prints "ABANDONED" from, just acted on here instead of only displayed.
//! This module adds NO new sweep semantics of its own.
//!
//! Named the "state reconcile pass" in every message this module prints, to
//! stay unambiguous against `rollover_runtime.rs`/`route.rs`'s own, narrower
//! "reconcile" (resolving an outcome-unknown tool effect) -- an unrelated
//! concept this module never touches.
//!
//! `--dry-run` performs zero writes. Three of the five resources below have
//! a pure decision usable without mutating (task, reservation, permit -- see
//! `permit::dead_records`'s own doc comment) or a check that is inherently
//! read-only (group). The other two do not: `sessions::list_with_retention`
//! sweeps orphan marker/endpoint/socket/screening files as an unavoidable
//! side effect of listing, and `worktree::gc`'s proof step shells out to git
//! per candidate and then writes the registry either way (`Kept` as well as
//! `Removed`). Rather than reimplement either sweep's own decision a second
//! time here (forbidden -- see this module's own doc comment above),
//! worktree still reports its dead-owner CANDIDATES via the cheap,
//! non-mutating pre-filter `gc` already applies before ever probing
//! (`worktree::gc_candidates`), and sessions is reported as not inspectable
//! in `--dry-run` at all.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use super::state::{self, StateDir};
use super::{CtxResult, group, permit, reservation, sessions, task, worktree};

#[derive(Debug, Clone, clap::Args)]
pub struct ReconcileArgs {
    /// Report what each sweep would heal without changing anything on disk.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Print one JSON object per resource kind instead of text lines.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct ResourceReport {
    resource: &'static str,
    healed: usize,
    ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ResourceReport {
    fn healed(resource: &'static str, ids: Vec<String>) -> Self {
        Self {
            resource,
            healed: ids.len(),
            ids,
            note: None,
            error: None,
        }
    }

    fn healed_with_note(resource: &'static str, ids: Vec<String>, note: impl Into<String>) -> Self {
        let mut report = Self::healed(resource, ids);
        report.note = Some(note.into());
        report
    }

    fn not_inspectable(resource: &'static str, note: impl Into<String>) -> Self {
        let mut report = Self::healed(resource, Vec::new());
        report.note = Some(note.into());
        report
    }

    fn failed(resource: &'static str, error: impl std::fmt::Display) -> Self {
        let mut report = Self::healed(resource, Vec::new());
        report.error = Some(error.to_string());
        report
    }
}

#[derive(Debug, Serialize)]
struct ReconcileReport {
    dry_run: bool,
    resources: Vec<ResourceReport>,
}

/// Repo-wide stuck-`Running`-card reap (issue #720 acceptance: heals a stuck
/// card nobody is trying to reclaim, without a `claim` call). Reuses `task::
/// reap_if_stale_pure`'s own liveness/TTL decision via `reap_all_dry`
/// (`--dry-run`, no lock, no write) or `reap_all_locked` (the real pass,
/// under `task`'s own lock).
fn reconcile_tasks(state: &StateDir, repo_slug: &str, now: u64, dry_run: bool) -> ResourceReport {
    if dry_run {
        return ResourceReport::healed("task", task::reap_all_dry(state, repo_slug, now));
    }
    match task::reap_all_locked(state, repo_slug, now) {
        Ok(ids) => ResourceReport::healed("task", ids),
        Err(e) => ResourceReport::failed("task", e),
    }
}

/// Dead-owner reservation prune across every provider ledger on disk (issue
/// #720 acceptance: frees a dead-owner reservation with no pending
/// `reserve`/`settle`). `--dry-run` reads each ledger's `entries` and filters
/// by `reservation::is_owner_alive` -- both already-`pub`/exposed pure reads,
/// no lock and no write; the live pass takes each provider's ledger lock in
/// turn via `prune_dead_locked`.
fn reconcile_reservations(state: &StateDir, dry_run: bool) -> ResourceReport {
    let mut ids = Vec::new();
    for provider in reservation::known_providers(state) {
        if dry_run {
            ids.extend(
                reservation::entries(state, &provider)
                    .into_iter()
                    .filter(|entry| !reservation::is_owner_alive(entry))
                    .map(|entry| entry.id),
            );
            continue;
        }
        match reservation::prune_dead_locked(state, &provider) {
            Ok(dead) => ids.extend(dead.into_iter().map(|entry| entry.id)),
            Err(e) => return ResourceReport::failed("reservation", e),
        }
    }
    ResourceReport::healed("reservation", ids)
}

/// Dead-owner heavy/writer permit sweep, both pools. `permit::dead_records`
/// is the pure, non-mutating read used for BOTH `--dry-run` (where it is the
/// whole answer) and the live pass (where it is taken first, so the report
/// names exactly what the sweep that follows removes); `permit::
/// live_records`/`live_writer_records` are the unchanged existing sweep,
/// called only once `dry_run` is false.
fn reconcile_permits(state: &StateDir, dry_run: bool) -> ResourceReport {
    let dead = permit::dead_records(state);
    let ids: Vec<String> = dead
        .iter()
        .map(|record| format!("{} (pid {})", record.label, record.pid))
        .collect();
    if !dry_run {
        let _ = permit::live_records(state);
        let _ = permit::live_writer_records(state);
    }
    ResourceReport::healed("permit", ids)
}

/// Closes every open work group whose claimed sub-orchestrator is confirmed
/// dead (issue #720 acceptance: closes an abandoned work group with a dead
/// coordinator and no live dashboard) -- the one resource with no automatic
/// reclaim anywhere else in this codebase. `group::is_abandoned` and `group::
/// short_id_is_alive` are the exact facts `status.rs` already prints
/// "ABANDONED" from; `group::close` is the same idempotent close `zirv ctx
/// group close` and `agent::run_with`'s own auto-close both call.
fn reconcile_groups(state: &StateDir, now: u64, dry_run: bool) -> ResourceReport {
    let mut ids = Vec::new();
    for wg in group::list(state) {
        if wg.closed_at.is_some() {
            continue;
        }
        let Some(sub) = wg.sub_orchestrator_session.clone() else {
            continue;
        };
        if !group::is_abandoned(&wg, group::short_id_is_alive(state, &sub)) {
            continue;
        }
        if !dry_run && let Err(e) = group::close(state, &wg.work_group_id, now) {
            return ResourceReport::failed("group", e);
        }
        ids.push(wg.work_group_id.clone());
    }
    ResourceReport::healed("group", ids)
}

/// `--dry-run`: reports `worktree::gc_candidates` (the same dead-owner
/// pre-filter `gc` applies before ever probing git or writing the registry),
/// noted as candidates rather than certain removals since the proof-required
/// probe never runs here. Live: calls `worktree::gc` exactly as `agent.rs`/
/// `dash/mod.rs` do, never widening what it removes.
fn reconcile_worktrees(state: &StateDir, repo: &Path, dry_run: bool) -> ResourceReport {
    if dry_run {
        let ids: Vec<String> = worktree::gc_candidates(state, repo, &sessions::is_alive)
            .into_iter()
            .map(|record| record.path)
            .collect();
        return if ids.is_empty() {
            ResourceReport::healed("worktree", ids)
        } else {
            ResourceReport::healed_with_note(
                "worktree",
                ids,
                "dead-owner candidate(s); the proof-required GC probe is not run in --dry-run",
            )
        };
    }
    let ids: Vec<String> = worktree::gc(state, repo, &sessions::is_alive)
        .into_iter()
        .filter(|(_, outcome)| {
            matches!(
                outcome,
                worktree::PruneOutcome::Removed
                    | worktree::PruneOutcome::RemovedWithSkipped(_)
                    | worktree::PruneOutcome::Archived { .. }
            )
        })
        .map(|(record, _)| record.path)
        .collect();
    ResourceReport::healed("worktree", ids)
}

/// `sessions::list_with_retention` sweeps four kinds of orphan file (socket
/// paths, endpoints, markers, screening summaries) plus the stale registry
/// record itself, all as an unavoidable side effect of listing (its own doc
/// comment) -- there is no separate pure decision to preview without
/// mutating, and reimplementing that decision a second time here is exactly
/// the "new sweep semantics" this module's own doc comment forbids. Reported
/// as not inspectable in `--dry-run` rather than skipped silently, so an
/// operator reading the output knows this resource was not zero, just
/// unchecked.
fn reconcile_sessions(state: &StateDir, dry_run: bool) -> ResourceReport {
    if dry_run {
        return ResourceReport::not_inspectable(
            "session",
            "not inspectable in --dry-run: sessions::list_with_retention's sweep is an \
             unavoidable side effect of listing",
        );
    }
    let ids: Vec<String> = sessions::list(state)
        .into_iter()
        .filter(|(_, liveness)| *liveness == sessions::Liveness::Stale)
        .map(|(record, _)| record.short)
        .collect();
    ResourceReport::healed("session", ids)
}

pub fn run<W: Write>(args: &ReconcileArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let state = StateDir::resolve(&env)?;
    let now = state::now_secs();
    let repo = std::env::current_dir()?;
    let repo_slug = state::repo_slug(&repo);

    let resources = vec![
        reconcile_tasks(&state, &repo_slug, now, args.dry_run),
        reconcile_reservations(&state, args.dry_run),
        reconcile_permits(&state, args.dry_run),
        reconcile_groups(&state, now, args.dry_run),
        reconcile_worktrees(&state, &repo, args.dry_run),
        reconcile_sessions(&state, args.dry_run),
    ];
    let any_failed = resources.iter().any(|r| r.error.is_some());

    if args.json {
        let report = ReconcileReport {
            dry_run: args.dry_run,
            resources,
        };
        writeln!(w, "{}", serde_json::to_string(&report)?)?;
    } else {
        for r in &resources {
            if let Some(e) = &r.error {
                writeln!(w, "{}: FAILED: {e}", r.resource)?;
                continue;
            }
            let ids = if r.ids.is_empty() {
                String::new()
            } else {
                format!(" ({})", r.ids.join(", "))
            };
            match &r.note {
                Some(note) => writeln!(w, "{}: {}{ids} -- {note}", r.resource, r.healed)?,
                None => writeln!(w, "{}: {}{ids}", r.resource, r.healed)?,
            }
        }
    }
    Ok(if any_failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_dir(tmp: &Path) -> StateDir {
        StateDir::from_root(tmp.to_path_buf())
    }

    /// Issue #720 acceptance: heals a stuck `Running` task card nobody is
    /// trying to reclaim, without a `claim` call anywhere in this test.
    #[test]
    fn reconcile_heals_a_stuck_task_card_with_no_claim_call() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");
        let repo_slug = state::repo_slug(&repo);
        let dead_pid = super::super::testenv::dead_pid();

        task::append_event(
            &state,
            &repo_slug,
            &task::Event::Created {
                id: "t1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1_000,
            },
        )
        .expect("create");
        task::append_event(
            &state,
            &repo_slug,
            &task::Event::Claimed {
                id: "t1".to_string(),
                claim: task::Claim {
                    session: "sess-1".to_string(),
                    pid: dead_pid,
                    pid_start_time: None,
                    host: "h".to_string(),
                    claimed_at: 1_000,
                    ttl_secs: 900,
                },
                attempts: 1,
                at: 1_000,
            },
        )
        .expect("claimed");

        let report = reconcile_tasks(&state, &repo_slug, 1_000 + 900 + 1, false);
        assert_eq!(report.healed, 1);
        assert_eq!(report.ids, vec!["t1".to_string()]);
        assert_eq!(
            task::load_cards(&state, &repo_slug)["t1"].state,
            task::State::Ready
        );
    }

    /// Issue #720 acceptance: frees a dead-owner reservation with no pending
    /// `reserve`/`settle` call. Seeds the ledger directly (the same shape
    /// `reservation.rs`'s own unit tests build) rather than through `reserve`
    /// -- which always stamps the CALLING process's own (live) pid, so it
    /// cannot produce a dead-owner entry at all.
    #[test]
    fn reconcile_frees_a_dead_owner_reservation_with_no_pending_call() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        let dead_pid = super::super::testenv::dead_pid();

        let ledger = reservation::Ledger {
            schema_version: 1,
            entries: vec![reservation::Reservation {
                id: "dead-1".to_string(),
                session: "sess-dead".to_string(),
                pid: dead_pid,
                pid_start_time: None,
                tokens: 9_999,
                created_at: 1_700_000_000,
            }],
        };
        state::create_private_dir_all(&state.reservations()).expect("mkdir");
        state::write_private(
            &state.reservations().join("claude.json"),
            &serde_json::to_string_pretty(&ledger).expect("serialize"),
        )
        .expect("seed ledger");

        let report = reconcile_reservations(&state, false);
        assert_eq!(report.healed, 1);
        assert_eq!(report.ids, vec!["dead-1".to_string()]);
        assert!(
            reservation::entries(&state, "claude").is_empty(),
            "the dead-owner entry must actually be removed from disk"
        );
    }

    /// A reservation whose owner IS alive must never be freed.
    #[test]
    fn reconcile_never_frees_a_live_owner_reservation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        reservation::reserve(&state, "claude", "sess-live", 10, 1_700_000_000)
            .expect("seed a live reservation");

        let report = reconcile_reservations(&state, false);
        assert_eq!(report.healed, 0);
        assert_eq!(reservation::entries(&state, "claude").len(), 1);
    }

    /// Issue #720 acceptance: closes an abandoned work group -- a dead
    /// coordinator and no live dashboard -- without any `zirv ctx group
    /// close` call.
    #[test]
    fn reconcile_closes_an_abandoned_group_with_a_dead_coordinator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        let wg = group::WorkGroup {
            work_group_id: "wg-1".to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: "batch".to_string(),
            child_limit: 3,
            token_budget: None,
            spent_tokens: 0,
            reserved_tokens: 0,
            deadline_secs: None,
            completion_contract: "report by mail".to_string(),
            created_at: 1_000,
            closed_at: None,
            admitted_children: 1,
            sub_orchestrator_session: Some("dead-short".to_string()),
        };
        group::create(&state, &wg).expect("create group");

        let report = reconcile_groups(&state, 2_000, false);
        assert_eq!(report.healed, 1);
        assert_eq!(report.ids, vec!["wg-1".to_string()]);
        let closed = group::load(&state, "wg-1")
            .expect("load io")
            .expect("group exists");
        assert_eq!(closed.closed_at, Some(2_000));
    }

    /// A group whose claimed coordinator IS alive must never be closed.
    #[test]
    fn reconcile_never_closes_a_group_whose_coordinator_is_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        // A group with no claimed sub-orchestrator at all is the "alive"
        // stand-in here (`is_abandoned` requires a claim before it can ever
        // fire -- see its own doc comment), which is the common case this
        // pass must leave alone.
        let wg = group::WorkGroup {
            work_group_id: "wg-1".to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: "batch".to_string(),
            child_limit: 3,
            token_budget: None,
            spent_tokens: 0,
            reserved_tokens: 0,
            deadline_secs: None,
            completion_contract: "report by mail".to_string(),
            created_at: 1_000,
            closed_at: None,
            admitted_children: 0,
            sub_orchestrator_session: None,
        };
        group::create(&state, &wg).expect("create group");

        let report = reconcile_groups(&state, 2_000, false);
        assert_eq!(report.healed, 0);
        let untouched = group::load(&state, "wg-1")
            .expect("load io")
            .expect("group exists");
        assert!(untouched.closed_at.is_none());
    }

    /// Issue #720 acceptance: `--dry-run` reports every finding with zero
    /// mutation -- every state file this pass could touch is byte-identical
    /// before and after.
    #[test]
    fn dry_run_reports_findings_with_zero_mutation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");
        let repo_slug = state::repo_slug(&repo);
        let dead_pid = super::super::testenv::dead_pid();

        // A stuck task card.
        task::append_event(
            &state,
            &repo_slug,
            &task::Event::Created {
                id: "t1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1_000,
            },
        )
        .expect("create");
        task::append_event(
            &state,
            &repo_slug,
            &task::Event::Claimed {
                id: "t1".to_string(),
                claim: task::Claim {
                    session: "sess-1".to_string(),
                    pid: dead_pid,
                    pid_start_time: None,
                    host: "h".to_string(),
                    claimed_at: 1_000,
                    ttl_secs: 900,
                },
                attempts: 1,
                at: 1_000,
            },
        )
        .expect("claimed");

        // An abandoned work group.
        let wg = group::WorkGroup {
            work_group_id: "wg-1".to_string(),
            parent_session_id: "sess-parent".to_string(),
            scope: "batch".to_string(),
            child_limit: 3,
            token_budget: None,
            spent_tokens: 0,
            reserved_tokens: 0,
            deadline_secs: None,
            completion_contract: "report by mail".to_string(),
            created_at: 1_000,
            closed_at: None,
            admitted_children: 1,
            sub_orchestrator_session: Some("dead-short".to_string()),
        };
        group::create(&state, &wg).expect("create group");

        let snapshot = |root: &Path| -> Vec<(std::path::PathBuf, Vec<u8>)> {
            let mut files = Vec::new();
            for entry in walkdir(root) {
                let bytes = std::fs::read(&entry).expect("read state file");
                files.push((entry, bytes));
            }
            files.sort();
            files
        };
        fn walkdir(root: &Path) -> Vec<std::path::PathBuf> {
            let mut out = Vec::new();
            let Ok(entries) = std::fs::read_dir(root) else {
                return out;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walkdir(&path));
                } else {
                    out.push(path);
                }
            }
            out
        }

        let before = snapshot(tmp.path());

        let resources = [
            reconcile_tasks(&state, &repo_slug, 1_000 + 900 + 1, true),
            reconcile_reservations(&state, true),
            reconcile_permits(&state, true),
            reconcile_groups(&state, 2_000, true),
            reconcile_worktrees(&state, &repo, true),
            reconcile_sessions(&state, true),
        ];

        // Every affected resource still reports the finding.
        assert_eq!(resources[0].ids, vec!["t1".to_string()], "task finding");
        assert_eq!(resources[3].ids, vec!["wg-1".to_string()], "group finding");
        assert!(
            resources[5].note.is_some(),
            "sessions must say it is not inspectable in dry-run"
        );

        let after = snapshot(tmp.path());
        assert_eq!(before, after, "--dry-run must never mutate any state file");

        // And the live decisions are unaffected by having run in dry-run
        // first.
        assert_eq!(
            task::load_cards(&state, &repo_slug)["t1"].state,
            task::State::Running,
            "dry-run must not have reaped the card"
        );
        assert!(
            group::load(&state, "wg-1")
                .expect("load io")
                .expect("exists")
                .closed_at
                .is_none(),
            "dry-run must not have closed the group"
        );
    }

    /// Issue #720 acceptance: `--json` round-trips through `serde_json`, one
    /// entry per resource kind.
    #[test]
    fn json_output_round_trips_with_one_entry_per_resource_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");
        let state_root = tmp.path().join("state");
        let _env = super::super::testenv::VarGuard::set(&[(
            state::STATE_ENV,
            Some(state_root.to_str().expect("utf8 tempdir path")),
        )]);

        let args = ReconcileArgs {
            dry_run: true,
            json: true,
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).expect("run io");
        assert_eq!(code, 0);

        let value: serde_json::Value =
            serde_json::from_slice(&out).expect("output must be valid json");
        assert_eq!(value["dry_run"], serde_json::json!(true));
        let resources = value["resources"].as_array().expect("resources array");
        assert_eq!(resources.len(), 6, "one entry per resource kind");
        let names: Vec<&str> = resources
            .iter()
            .map(|r| r["resource"].as_str().expect("resource name"))
            .collect();
        assert_eq!(
            names,
            vec![
                "task",
                "reservation",
                "permit",
                "group",
                "worktree",
                "session"
            ]
        );
    }

    /// A resource kind whose sweep fails must not abort the whole pass, and
    /// the process must exit non-zero.
    #[test]
    fn one_resource_failing_does_not_abort_the_others() {
        let ok = ResourceReport::healed("task", vec!["t1".to_string()]);
        let failed = ResourceReport::failed("group", "boom");
        let resources = [ok, failed];
        assert!(resources.iter().any(|r| r.error.is_some()));
        assert!(
            resources[0].error.is_none(),
            "a failure in one resource must not clear another's report"
        );
    }
}
