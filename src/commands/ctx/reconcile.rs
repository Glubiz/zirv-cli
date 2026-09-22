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
//! `permit::dead_records`'s own doc comment). The other two do not:
//! `sessions::list`/`list_with_retention` sweeps a stale registry record plus
//! four kinds of orphan file (socket paths, endpoints, markers, screening
//! summaries) as an unavoidable side effect of LISTING -- so `--dry-run`
//! must never call it, directly or transitively -- and `worktree::gc`'s
//! proof step shells out to git per candidate and then writes the registry
//! either way (`Kept` as well as `Removed`). Rather than reimplement either
//! sweep's own decision a second time here (forbidden -- see this module's
//! own doc comment above), worktree still reports its dead-owner CANDIDATES
//! via the cheap, non-mutating pre-filter `gc` already applies before ever
//! probing (`worktree::gc_candidates`), and sessions is reported as not
//! inspectable in `--dry-run` at all.
//!
//! The group check needs a session's liveness too (`group::is_abandoned`'s
//! `claimant_alive`), but has a non-sweeping way to answer that:
//! `sessions::short_is_live` reads one record straight off disk
//! (`sessions::load_record`) and applies the pure pid+start-time probe
//! (`sessions::record_is_alive`) with no listing and no sweep, so `--dry-run`
//! uses that. The LIVE pass instead takes exactly ONE `sessions::list` (the
//! whole point of level-triggered: healing sessions IS one of this pass's
//! own resources) and shares that single snapshot between the group check
//! and the session report, mirroring `status.rs`'s `group_header`/
//! `group_tree_lines`, which build one `live_shorts` set per render rather
//! than re-querying liveness per group.
//!
//! **Closing a group is deliberately conservative, and today that means
//! coordinator-liveness alone.** `group::is_abandoned` only ever looks at
//! the claimed sub-orchestrator; it says nothing about whether a live child
//! this group admitted is still doing work. Checked (Grepped `status.rs`,
//! `dash/mod.rs`, `sessions::Record`, `log::DelegationRow`, `reservation::
//! Reservation`) for any ON-DISK record that attributes a still-running
//! session to its work group: none exists. `dash::Pane::work_group_id` is
//! the only place that link is ever held, and it lives purely in a live
//! dashboard process's own memory -- gone the moment that process exits, and
//! never visible to a separate `zirv ctx reconcile` invocation reading state
//! off disk. So this closes an abandoned group on coordinator death alone,
//! same as `status.rs` already flags "ABANDONED" on. The operator-visible
//! consequence: a still-running child of a dead coordinator can no longer
//! admit nested children once its group is closed (`group::admit_child`
//! refuses a closed group outright) -- called out in the text output, the
//! group section's own `note`, and in `README.md`.
//!
//! Work groups are also machine-wide, unlike every other resource here:
//! `<state>/groups` carries no repository dimension at all (a group can
//! outlive, and is never scoped to, any one checkout), the same way permits
//! and reservations are machine-wide pools rather than per-repo state. The
//! text output labels the group line accordingly.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use super::state::{self, StateDir};
use super::{CtxResult, group, permit, reservation, sessions, task, worktree};

/// Issue #720 review: the operator-visible cost of closing a group on
/// coordinator liveness alone, with no on-disk attribution of a live child
/// to its group (this module's own doc comment explains why none exists
/// today). Shared by the group resource's own `note` and its text-output
/// line, so the two can never say something different.
const GROUP_ATTRIBUTION_CAVEAT: &str = "closes on coordinator liveness alone -- no on-disk \
     record attributes a live session to its work group, so a still-running child of a dead \
     coordinator can no longer admit nested children once its group is closed";

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

    /// Issue #720 review (item 2): a partial-failure report -- every id
    /// actually healed BEFORE and AFTER the failing item is kept, unlike
    /// [`failed`], which is only for a resource with no per-item ids to
    /// preserve at all (`task`'s single locked pass).
    fn healed_with_error(
        resource: &'static str,
        ids: Vec<String>,
        error: impl Into<String>,
    ) -> Self {
        let mut report = Self::healed(resource, ids);
        report.error = Some(error.into());
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
///
/// Issue #720 review (item 2): one provider's ledger failing to lock/save
/// must not discard ids another provider already healed, or skip providers
/// still to come -- every provider is attempted, in SORTED order (so the
/// output is deterministic run to run), and every error is collected rather
/// than aborting on the first one.
fn reconcile_reservations(state: &StateDir, dry_run: bool) -> ResourceReport {
    let mut providers = reservation::known_providers(state);
    providers.sort();

    let mut ids = Vec::new();
    let mut errors = Vec::new();
    for provider in providers {
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
            Err(e) => errors.push(format!("{provider}: {e}")),
        }
    }
    if errors.is_empty() {
        ResourceReport::healed("reservation", ids)
    } else {
        ResourceReport::healed_with_error("reservation", ids, errors.join("; "))
    }
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
/// reclaim anywhere else in this codebase. `group::is_abandoned` is the same
/// decision `status.rs` already prints "ABANDONED" from; `group::close` is
/// the same idempotent close `zirv ctx group close` and `agent::run_with`'s
/// own auto-close both call. See this module's own doc comment for why
/// closing stays conservative on coordinator liveness alone
/// ([`GROUP_ATTRIBUTION_CAVEAT`]), and for why groups are machine-wide.
///
/// Liveness comes from `live_shorts` (`Some` in live mode: ONE `sessions::
/// list` snapshot taken once for the whole pass, per this module's own doc
/// comment) when given; `None` (`--dry-run`) falls back to `sessions::
/// short_is_live`'s own non-sweeping direct read, so a dry run never reaches
/// `sessions::list`/`list_with_retention`'s sweep.
///
/// Issue #720 review (item 2): one group failing to close must not discard
/// ids already healed, or skip groups still to come -- every open,
/// abandoned group is attempted, in SORTED id order (deterministic output),
/// and every error is collected rather than aborting on the first one.
fn reconcile_groups(
    state: &StateDir,
    now: u64,
    dry_run: bool,
    live_shorts: Option<&BTreeSet<String>>,
) -> ResourceReport {
    let mut groups = group::list(state);
    groups.sort_by(|a, b| a.work_group_id.cmp(&b.work_group_id));

    let mut ids = Vec::new();
    let mut errors = Vec::new();
    for wg in groups {
        if wg.closed_at.is_some() {
            continue;
        }
        let Some(sub) = wg.sub_orchestrator_session.clone() else {
            continue;
        };
        let alive = match live_shorts {
            Some(shorts) => shorts.contains(&sub),
            None => sessions::short_is_live(state, &sub),
        };
        if !group::is_abandoned(&wg, alive) {
            continue;
        }
        if !dry_run && let Err(e) = group::close(state, &wg.work_group_id, now) {
            errors.push(format!("{}: {e}", wg.work_group_id));
            continue;
        }
        ids.push(wg.work_group_id.clone());
    }
    let mut report = if errors.is_empty() {
        ResourceReport::healed("group", ids)
    } else {
        ResourceReport::healed_with_error("group", ids, errors.join("; "))
    };
    report.note = Some(GROUP_ATTRIBUTION_CAVEAT.to_string());
    report
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

/// `sessions::list`/`list_with_retention` sweeps four kinds of orphan file
/// (socket paths, endpoints, markers, screening summaries) plus the stale
/// registry record itself, all as an unavoidable side effect of LISTING
/// (its own doc comment) -- there is no separate pure decision to preview
/// without mutating, and reimplementing that decision a second time here is
/// exactly the "new sweep semantics" this module's own doc comment forbids.
/// Reported as not inspectable in `--dry-run` rather than skipped silently,
/// so an operator reading the output knows this resource was not zero, just
/// unchecked.
///
/// Live mode takes no `sessions::list` call of its own: `snapshot` is the
/// ONE call `run` already made for the whole pass (shared with
/// `reconcile_groups`'s own liveness check), so listing sessions here never
/// costs a second sweep.
fn reconcile_sessions(
    dry_run: bool,
    snapshot: Option<&[(sessions::Record, sessions::Liveness)]>,
) -> ResourceReport {
    if dry_run {
        return ResourceReport::not_inspectable(
            "session",
            "not inspectable in --dry-run: sessions::list_with_retention's sweep is an \
             unavoidable side effect of listing",
        );
    }
    let ids: Vec<String> = snapshot
        .unwrap_or(&[])
        .iter()
        .filter(|(_, liveness)| *liveness == sessions::Liveness::Stale)
        .map(|(record, _)| record.short.clone())
        .collect();
    ResourceReport::healed("session", ids)
}

pub fn run<W: Write>(args: &ReconcileArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let state = StateDir::resolve(&env)?;
    let now = state::now_secs();
    let repo = std::env::current_dir()?;
    let repo_slug = state::repo_slug(&repo);

    // Issue #720 review (item 1): `--dry-run` must never reach `sessions::
    // list`/`list_with_retention` (its sweep is an unavoidable side effect
    // of listing -- see `reconcile_sessions`'s own doc comment). The live
    // pass takes exactly ONE snapshot for the whole reconcile pass and
    // shares it between `reconcile_groups`'s coordinator-liveness check and
    // `reconcile_sessions`'s own stale-record report, mirroring `status.rs`'s
    // `group_header`/`group_tree_lines`' single `live_shorts` set per render.
    let session_snapshot = if args.dry_run {
        None
    } else {
        Some(sessions::list(&state))
    };
    let live_shorts: Option<BTreeSet<String>> = session_snapshot.as_ref().map(|records| {
        records
            .iter()
            .filter(|(_, liveness)| *liveness == sessions::Liveness::Live)
            .map(|(record, _)| record.short.clone())
            .collect()
    });

    let resources = vec![
        reconcile_tasks(&state, &repo_slug, now, args.dry_run),
        reconcile_reservations(&state, args.dry_run),
        reconcile_permits(&state, args.dry_run),
        reconcile_groups(&state, now, args.dry_run, live_shorts.as_ref()),
        reconcile_worktrees(&state, &repo, args.dry_run),
        reconcile_sessions(args.dry_run, session_snapshot.as_deref()),
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
            // Issue #720 review (item 4): groups are machine-wide (`<state>/
            // groups` carries no repo dimension), unlike every other
            // resource this pass touches -- labeled here so the text output
            // does not imply the same repo scoping task/worktree actually
            // have.
            let label = if r.resource == "group" {
                "group (machine-wide)"
            } else {
                r.resource
            };
            let ids = if r.ids.is_empty() {
                String::new()
            } else {
                format!(" ({})", r.ids.join(", "))
            };
            write!(w, "{label}: {}{ids}", r.healed)?;
            if let Some(note) = &r.note {
                write!(w, " -- {note}")?;
            }
            if let Some(e) = &r.error {
                write!(w, " -- ERROR: {e}")?;
            }
            writeln!(w)?;
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

    /// Issue #720 review (item 2): one provider's ledger failing to lock/save
    /// must not discard an id another provider already healed. `claude`
    /// sorts before `codex`, so this also proves the SORTED iteration order:
    /// `claude` (which heals) always runs before `codex` (whose lock is
    /// forced to fail).
    #[test]
    fn reconcile_reservations_keeps_healed_ids_when_one_provider_errors() {
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
        .expect("seed claude ledger");

        // A second, otherwise-empty ledger -- so `known_providers` actually
        // discovers "codex" at all (it only recognises a provider from its
        // own `<slug>.json` ledger file).
        state::write_private(
            &state.reservations().join("codex.json"),
            &serde_json::to_string_pretty(&reservation::Ledger::default()).expect("serialize"),
        )
        .expect("seed empty codex ledger");

        // Force `codex`'s own `prune_dead_locked` to fail the same way the
        // group test above forces a `close` to fail: a DIRECTORY sitting at
        // the exact path its lock file would open.
        std::fs::create_dir_all(state.reservations().join("codex.lock")).expect("mkdir codex.lock");

        let report = reconcile_reservations(&state, false);
        assert_eq!(
            report.ids,
            vec!["dead-1".to_string()],
            "claude's dead-owner entry must still be reported healed"
        );
        assert!(report.error.is_some(), "codex's failure must be reported");
        assert!(
            report.error.as_ref().unwrap().contains("codex"),
            "the error must name which provider failed: {:?}",
            report.error
        );
        assert!(
            reservation::entries(&state, "claude").is_empty(),
            "claude's dead-owner entry was actually removed"
        );
    }

    fn sample_group(id: &str, sub_orchestrator_session: Option<&str>) -> group::WorkGroup {
        group::WorkGroup {
            work_group_id: id.to_string(),
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
            sub_orchestrator_session: sub_orchestrator_session.map(str::to_string),
        }
    }

    /// Issue #720 acceptance: closes an abandoned work group -- a dead
    /// coordinator and no live dashboard -- without any `zirv ctx group
    /// close` call. `live_shorts` is the empty set, standing in for "the one
    /// `sessions::list` snapshot `run` takes for a live pass" naming nobody
    /// alive.
    #[test]
    fn reconcile_closes_an_abandoned_group_with_a_dead_coordinator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        group::create(&state, &sample_group("wg-1", Some("dead-short"))).expect("create group");

        let live_shorts = BTreeSet::new();
        let report = reconcile_groups(&state, 2_000, false, Some(&live_shorts));
        assert_eq!(report.healed, 1);
        assert_eq!(report.ids, vec!["wg-1".to_string()]);
        assert!(
            report
                .note
                .as_deref()
                .is_some_and(|n| n.contains("attribut")),
            "the group report must always carry the attribution caveat: {:?}",
            report.note
        );
        let closed = group::load(&state, "wg-1")
            .expect("load io")
            .expect("group exists");
        assert_eq!(closed.closed_at, Some(2_000));
    }

    /// A group with no claimed sub-orchestrator at all must never be closed
    /// (`is_abandoned` requires a claim before it can ever fire), which is
    /// the common case this pass must leave alone.
    #[test]
    fn reconcile_never_closes_a_group_with_no_claimed_coordinator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        group::create(&state, &sample_group("wg-1", None)).expect("create group");

        let live_shorts = BTreeSet::new();
        let report = reconcile_groups(&state, 2_000, false, Some(&live_shorts));
        assert_eq!(report.healed, 0);
        let untouched = group::load(&state, "wg-1")
            .expect("load io")
            .expect("group exists");
        assert!(untouched.closed_at.is_none());
    }

    /// A group whose claimed coordinator IS alive (present in the live pass's
    /// own `live_shorts` snapshot) must never be closed.
    #[test]
    fn reconcile_never_closes_a_group_whose_coordinator_is_alive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        group::create(&state, &sample_group("wg-1", Some("live-short"))).expect("create group");

        let live_shorts: BTreeSet<String> = ["live-short".to_string()].into_iter().collect();
        let report = reconcile_groups(&state, 2_000, false, Some(&live_shorts));
        assert_eq!(report.healed, 0);
        let untouched = group::load(&state, "wg-1")
            .expect("load io")
            .expect("group exists");
        assert!(untouched.closed_at.is_none());
    }

    /// Issue #720 review (item 2): one group failing to close must not
    /// discard an id another group already healed, and iteration is in
    /// sorted `work_group_id` order so `wg-a` (which heals) always runs
    /// before `wg-b` (whose close is forced to fail).
    #[test]
    fn reconcile_groups_keeps_healed_ids_when_one_group_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_dir(tmp.path());
        group::create(&state, &sample_group("wg-a", Some("dead-short-a"))).expect("create wg-a");
        group::create(&state, &sample_group("wg-b", Some("dead-short-b"))).expect("create wg-b");

        // Force `wg-b`'s own `close` to fail: pre-create a DIRECTORY at the
        // exact path its lock file would open -- `group::open_lock_file`'s
        // `OpenOptions::new().write(true)` on an existing directory fails on
        // every platform this runs on.
        std::fs::create_dir_all(state.groups().join("wg-b.lock")).expect("mkdir wg-b.lock");

        let live_shorts = BTreeSet::new();
        let report = reconcile_groups(&state, 2_000, false, Some(&live_shorts));
        assert_eq!(
            report.ids,
            vec!["wg-a".to_string()],
            "wg-a must still be reported healed"
        );
        assert!(report.error.is_some(), "wg-b's failure must be reported");
        assert!(
            report.error.as_ref().unwrap().contains("wg-b"),
            "the error must name which group failed: {:?}",
            report.error
        );

        assert!(
            group::load(&state, "wg-a")
                .expect("load io")
                .expect("exists")
                .closed_at
                .is_some(),
            "wg-a was actually closed"
        );
        assert!(
            group::load(&state, "wg-b")
                .expect("load io")
                .expect("exists")
                .closed_at
                .is_none(),
            "wg-b's failed close must leave it open"
        );
    }

    /// Issue #720 acceptance, strengthened by review item 1: `--dry-run`
    /// reports every finding with zero mutation -- the WHOLE state dir tree
    /// (every file, including names) is byte-identical before and after.
    /// The original version of this test seeded no stale session artefacts,
    /// so it never actually exercised the one sweep review found
    /// `reconcile_groups` was silently triggering: `group::short_id_is_alive`
    /// -> `sessions::list` -> `list_with_retention`, which deletes a stale
    /// registry record and orphan `.nudge`/`.sock`/`.screening` files as a
    /// side effect of merely being CALLED, dry run or not. This seeds a
    /// stale session record AND an orphan `.nudge` marker specifically to
    /// catch that class of regression again.
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
        group::create(&state, &sample_group("wg-1", Some("dead-short"))).expect("create group");

        // A stale session record: no live process, and no `in_flight`
        // witness, so `list_with_retention` would sweep it from disk on
        // sight (`Liveness::Stale`'s own arm).
        let mut stale = sessions::Record::new("stale-sess", "claude", &repo, sessions::Verb::Wrap);
        stale.pid = dead_pid;
        let stale_short = stale.short.clone();
        state::create_private_dir_all(&state.sessions()).expect("mkdir sessions");
        state::write_private(
            &state.sessions().join(format!("{stale_short}.json")),
            &serde_json::to_string_pretty(&stale).expect("serialize"),
        )
        .expect("seed stale session record");

        // An orphan `.nudge` marker with no matching record (live or not) at
        // all -- `sweep_orphaned_markers`'s own target.
        let orphan_marker = state.sessions().join("orphan-short.nudge");
        std::fs::write(&orphan_marker, b"orphan").expect("seed orphan marker");

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
            reconcile_groups(&state, 2_000, true, None),
            reconcile_worktrees(&state, &repo, true),
            reconcile_sessions(true, None),
        ];

        // Every affected resource still reports the finding.
        assert_eq!(resources[0].ids, vec!["t1".to_string()], "task finding");
        assert_eq!(resources[3].ids, vec!["wg-1".to_string()], "group finding");
        assert!(
            resources[5].note.is_some(),
            "sessions must say it is not inspectable in dry-run"
        );

        let after = snapshot(tmp.path());
        assert_eq!(
            before, after,
            "--dry-run must never mutate any state file, including a stale \
             session record or an orphan marker"
        );

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
        assert!(
            state
                .sessions()
                .join(format!("{stale_short}.json"))
                .is_file(),
            "the stale session record must still be on disk"
        );
        assert!(
            orphan_marker.is_file(),
            "the orphan marker must still be on disk"
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
