//! Per-session record of the workflow step last shown to a session, and
//! detection of workflows nobody is advancing (#878).

use std::path::Path;

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{KEEP_NEWEST, StateDir, create_private_dir_all, write_private};

use super::definitions::WorkflowStatus;
use super::state::{WorkflowState, load_active_for_session, load_from_path, repo_dir};

/// A workflow with no completed step is stale after this much idle time even when its session looks alive.
pub const STALE_WORKFLOW_SECS: u64 = 3 * 24 * 60 * 60;

/// Identity of what a session was last shown: workflow, step index and status.
pub fn step_marker(state: &WorkflowState) -> String {
    format!("{}:{}:{:?}", state.id, state.current_step, state.status)
}

fn marker_dir(state_dir: &StateDir) -> std::path::PathBuf {
    state_dir.root().join("workflow-injected")
}

fn marker_path(state_dir: &StateDir, short: &str) -> std::path::PathBuf {
    marker_dir(state_dir).join(format!(
        "{:016x}",
        crate::commands::ctx::event::input_hash(short)
    ))
}

/// Best-effort: remember that `short` was shown `state`'s current step.
pub fn record_injected(state_dir: &StateDir, short: &str, state: &WorkflowState) {
    let dir = marker_dir(state_dir);
    if create_private_dir_all(&dir).is_err() {
        return;
    }
    if write_private(&marker_path(state_dir, short), &step_marker(state)).is_ok() {
        crate::commands::ctx::state::prune_to_newest(&dir, KEEP_NEWEST);
    }
}

fn last_injected(state_dir: &StateDir, short: &str) -> Option<String> {
    std::fs::read_to_string(marker_path(state_dir, short)).ok()
}

/// The current step's context for the workflow bound to `short`, only when it differs from what the
/// session was last shown; records the new marker. `None` when nothing is bound, nothing changed, or
/// rendering fails.
pub fn changed_step_context(
    state_dir: &StateDir,
    repo: &Path,
    short: &str,
    max_bytes: usize,
) -> Option<String> {
    crate::commands::ctx::sessions::workflow_id_for(state_dir, short)?;
    let state = load_active_for_session(state_dir, repo, short).ok()??;
    if last_injected(state_dir, short).as_deref() == Some(step_marker(&state).as_str()) {
        return None;
    }
    step_context_note(state_dir, short, &state, repo, max_bytes, false)
}

/// `state`'s current step context behind a one-line header, capped; records the marker on success.
/// `started` words the header for a workflow that was just started rather than advanced.
pub fn step_context_note(
    state_dir: &StateDir,
    short: &str,
    state: &WorkflowState,
    repo: &Path,
    max_bytes: usize,
    started: bool,
) -> Option<String> {
    let context =
        super::lifecycle::render_current_context(state, repo, dirs::home_dir().as_deref())
            .ok()
            .flatten()?;
    record_injected(state_dir, short, state);
    let step = state.current().map(|s| s.id.as_str()).unwrap_or("-");
    let lead = if started {
        "Started workflow"
    } else {
        "Workflow"
    };
    Some(super::lifecycle::cap_workflow_context(
        format!(
            "[zirv workflow] {lead} {} for this session; current step '{step}' ({:?}). Follow it:\n{context}",
            state.id, state.status
        ),
        max_bytes,
    ))
}

/// A Running or AwaitingApproval workflow nobody is advancing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedWorkflow {
    pub id: String,
    /// Close reason recorded when swept.
    pub reason: String,
    /// Compact form for status, e.g. `3d idle` or `session ended`.
    pub brief: String,
    /// The asking session's own workflow: reported, never swept.
    pub this_session: bool,
}

/// This repo's in-flight workflows with zero completed steps that are abandoned: bound to a session
/// that has ended, or idle for [`STALE_WORKFLOW_SECS`]. `current_session` (a short id) is reported
/// with `this_session` set whenever its own workflow has not advanced. Read-only.
pub fn abandoned_workflows(
    state_dir: &StateDir,
    repo: &Path,
    current_session: Option<&str>,
    now: u64,
) -> CtxResult<Vec<AbandonedWorkflow>> {
    let bindings = crate::commands::ctx::sessions::workflow_bindings(state_dir);
    let dir = repo_dir(state_dir, repo);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(state) = load_from_path(&path, id) else {
            continue;
        };
        if !matches!(
            state.status,
            WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
        ) || !state.completed_steps.is_empty()
        {
            continue;
        }
        let owner = bindings
            .iter()
            .find(|(_, bound)| *bound == state.id)
            .map(|(short, _)| short.as_str());
        let idle = now.saturating_sub(state.updated_at);
        let stale = idle >= STALE_WORKFLOW_SECS;
        let days = idle / 86_400;
        let item = |reason: String, brief: String, this_session: bool| AbandonedWorkflow {
            id: state.id.clone(),
            reason,
            brief,
            this_session,
        };
        if owner.is_some() && owner == current_session {
            found.push(item(
                "this session, not advanced".to_string(),
                "this session, not advanced".to_string(),
                true,
            ));
        } else if let Some(short) = owner
            && !crate::commands::ctx::sessions::short_is_live(state_dir, short)
        {
            found.push(item(
                format!("abandoned: session {short} ended without advancing"),
                "session ended".to_string(),
                false,
            ));
        } else if stale {
            found.push(item(
                format!("stale: no advance in {days}d"),
                format!("{days}d idle"),
                false,
            ));
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::commands::ctx::sessions::{Record, SessionGuard, Verb, bind_workflow_id, short_id};
    use crate::commands::workflow::engine::tests::low_classification;
    use crate::commands::workflow::engine::{WorkflowKind, save, save_preserving_active};

    use super::*;

    fn fresh(repo: &Path) -> WorkflowState {
        WorkflowState::start(
            repo.to_path_buf(),
            "fix the thing".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        )
    }

    #[test]
    fn marker_changes_with_step_and_status() {
        let repo = tempdir().expect("repo");
        let mut state = fresh(repo.path());
        let first = step_marker(&state);
        state.current_step += 1;
        assert_ne!(first, step_marker(&state));
        let second = step_marker(&state);
        state.status = WorkflowStatus::AwaitingApproval;
        assert_ne!(second, step_marker(&state));
    }

    /// The launch compile site (`active_skill_context`) records what it injected, so the first prompt
    /// hook does not repeat it, and a later step change is still reported once.
    #[test]
    fn compile_site_marker_suppresses_a_duplicate_then_a_step_change_injects_once() {
        let state_dir = tempdir().expect("state");
        let store = StateDir::from_root(state_dir.path().to_path_buf());
        let repo = tempdir().expect("repo");
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var(crate::commands::ctx::state::STATE_ENV, state_dir.path());
        }
        let mut workflow = fresh(repo.path());
        workflow.status = WorkflowStatus::Running;
        save(&store, &workflow, true).expect("save");
        let session = "compile0000000000";
        let short = short_id(session);
        let _guard = SessionGuard::register(
            &store,
            Record::new(session, "claude", repo.path(), Verb::Chat),
        );
        bind_workflow_id(&store, &short, &workflow.id);

        let composed =
            crate::commands::workflow::engine::active_skill_context(repo.path(), Some(&short))
                .expect("renders");
        assert!(composed.is_some(), "the compile site injects the step");
        assert_eq!(
            changed_step_context(&store, repo.path(), &short, 64_000),
            None,
            "the first prompt must not repeat what the composed prompt carried"
        );

        workflow.completed_steps.push(workflow.steps[0].id.clone());
        workflow.current_step += 1;
        save(&store, &workflow, true).expect("save");
        let moved = changed_step_context(&store, repo.path(), &short, 64_000)
            .expect("a moved step is injected");
        assert!(
            moved.contains(&format!("step: {}", workflow.steps[1].id)),
            "{moved}"
        );
        assert_eq!(
            changed_step_context(&store, repo.path(), &short, 64_000),
            None
        );
    }

    #[test]
    fn detects_dead_session_and_stale_workflows_but_not_a_live_fresh_one() {
        let state_dir = tempdir().expect("state");
        let store = StateDir::from_root(state_dir.path().to_path_buf());
        let repo = tempdir().expect("repo");

        // Bound to a session whose record is not alive (a pid above any OS pid limit).
        let mut dead = fresh(repo.path());
        dead.id = "w-dead".into();
        save(&store, &dead, false).expect("save");
        let mut record = Record::new("deaddead00000000", "claude", repo.path(), Verb::Chat);
        record.pid = 4_200_000;
        record.start_time = None;
        let dead_short = short_id("deaddead00000000");
        let _dead_guard = SessionGuard::register(&store, record);
        bind_workflow_id(&store, &dead_short, &dead.id);

        // Unbound and idle for four days.
        let mut stale = fresh(repo.path());
        stale.id = "w-stale".into();
        stale.updated_at = 1_000;
        save_preserving_active(&store, &stale).expect("save");

        // Bound to this live process and fresh.
        let mut live = fresh(repo.path());
        live.id = "w-live".into();
        save_preserving_active(&store, &live).expect("save");
        let live_session = "livelive00000000";
        let _guard = SessionGuard::register(
            &store,
            Record::new(live_session, "claude", repo.path(), Verb::Chat),
        );
        bind_workflow_id(&store, &short_id(live_session), &live.id);

        let now = stale.updated_at + STALE_WORKFLOW_SECS + 86_400;
        let mut live_state = live.clone();
        live_state.updated_at = now;
        save_preserving_active(&store, &live_state).expect("save");
        let mut dead_state = dead.clone();
        dead_state.updated_at = now;
        save_preserving_active(&store, &dead_state).expect("save");

        let found = abandoned_workflows(&store, repo.path(), None, now).expect("scan");
        let ids: Vec<_> = found.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["w-dead", "w-stale"], "{found:?}");
        assert!(found[0].reason.contains(&dead_short), "{found:?}");
        assert!(found[1].reason.starts_with("stale: no advance in"));

        // The asking session's own unadvanced workflow is reported but flagged.
        let own = abandoned_workflows(&store, repo.path(), Some(&short_id(live_session)), now)
            .expect("scan");
        assert!(own.iter().any(|w| w.id == "w-live" && w.this_session));
    }
}
