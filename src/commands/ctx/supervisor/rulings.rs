//! The supervisor's ruling store: binding verdicts that only ever narrow.
//!
//! A ruling can block a step, require a revision, stop a retry or pick one of the options the
//! seat offered. It never writes code, answers a permission request, grants anything or widens
//! scope. Each kind has one strict reply format; anything else yields no ruling.

use serde::{Deserialize, Serialize};

use super::super::state::{self, StateDir, now_secs};
use super::{CtxResult, lock_beside};

const DIR: &str = "supervisor/rulings";
const FILE: &str = "rulings.json";
const KEEP: usize = 200;
/// The Stop hook blocks at most this many times per ruling, so it can never loop forever.
pub(crate) const MAX_STOP_BLOCKS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulingKind {
    Plan,
    Done,
    Retry,
    Choice,
}

impl RulingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Done => "done",
            Self::Retry => "retry",
            Self::Choice => "choice",
        }
    }

    /// The reply format the consult is told to use for this kind.
    pub(crate) fn reply_format(self) -> &'static str {
        match self {
            Self::Plan => "APPROVE, or REVISE: <reasons>",
            Self::Done => "DONE, or NOT_DONE: <what is missing>",
            Self::Retry => "RETRY, or STOP: <reason>",
            Self::Choice => "CHOICE: <option number>, then optionally a line REASON: <why>",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulingStatus {
    Open,
    Resolved,
    Overridden,
    /// The superseding consult could not run, so the ruling stopped binding.
    Lapsed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    pub id: String,
    pub session: String,
    pub workflow: Option<String>,
    pub kind: RulingKind,
    pub verdict: String,
    /// Redacted and capped before it is stored.
    pub reason: String,
    pub ts: u64,
    pub status: RulingStatus,
    /// How many times the Stop hook has blocked on this ruling.
    #[serde(default)]
    pub blocks: u32,
    #[serde(default)]
    pub override_reason: Option<String>,
    #[serde(default)]
    pub lapse_reason: Option<String>,
    /// How many times the workflow gate has refused a step because of this ruling.
    #[serde(default)]
    pub refusals: u32,
}

/// Verdicts that bind until resolved or overridden; the rest are recorded as already resolved.
fn binds(verdict: &str) -> bool {
    matches!(verdict, "revise" | "not_done" | "stop")
}

/// Parse one kind's strict reply into `(verdict, reason)`. Anything unparseable is `None`.
/// For a choice the verdict is the chosen option's own text.
pub(crate) fn parse_reply(
    kind: RulingKind,
    reply: &str,
    options: &[String],
) -> Option<(String, String)> {
    let reply = reply.trim();
    let (head, rest) = reply.split_once('\n').unwrap_or((reply, ""));
    let (head, rest) = (head.trim(), rest.trim());
    let pair = |good: &str, good_verdict: &str, bad: &str, bad_verdict: &str| {
        if head == good {
            return Some((good_verdict.to_string(), rest.to_string()));
        }
        let reasons = head.strip_prefix(bad)?.strip_prefix(':')?.trim();
        let reason = [reasons, rest]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (!reasons.is_empty()).then(|| (bad_verdict.to_string(), reason))
    };
    match kind {
        RulingKind::Plan => pair("APPROVE", "approve", "REVISE", "revise"),
        RulingKind::Done => pair("DONE", "done", "NOT_DONE", "not_done"),
        RulingKind::Retry => pair("RETRY", "retry", "STOP", "stop"),
        RulingKind::Choice => {
            let number: usize = head.strip_prefix("CHOICE:")?.trim().parse().ok()?;
            let option = options.get(number.checked_sub(1)?)?;
            let reason = rest.strip_prefix("REASON:").unwrap_or("").trim();
            Some((option.clone(), reason.to_string()))
        }
    }
}

fn path(state: &StateDir) -> std::path::PathBuf {
    state.root().join(DIR).join(FILE)
}

fn load(path: &std::path::Path) -> Vec<Ruling> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(path: &std::path::Path, rulings: &[Ruling]) -> bool {
    let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(rulings)) else {
        return false;
    };
    let _ = state::create_private_dir_all(dir);
    state::write_private(path, &text).is_ok()
}

/// Run `f` over the stored rulings under the store lock and save the result.
fn with_store<T>(state: &StateDir, f: impl FnOnce(&mut Vec<Ruling>) -> T) -> Option<T> {
    let path = path(state);
    let _lock = lock_beside(&path)?;
    let mut rulings = load(&path);
    let out = f(&mut rulings);
    if rulings.len() > KEEP {
        rulings.drain(..rulings.len() - KEEP);
    }
    save(&path, &rulings).then_some(out)
}

/// Record a ruling. It supersedes any open ruling of the same kind for the same session and
/// workflow, so a later `done` or a re-advanced plan lifts the earlier block.
pub(crate) fn record(
    state: &StateDir,
    session: &str,
    workflow: Option<&str>,
    kind: RulingKind,
    verdict: &str,
    reason: &str,
) -> Option<Ruling> {
    let ts = now_secs();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let digest = crate::commands::workflow::engine::hash_bytes(
        format!("{session}{workflow:?}{}{verdict}{ts}{nanos}", kind.as_str()).as_bytes(),
    );
    let ruling = Ruling {
        id: digest[..8].to_string(),
        session: session.to_string(),
        workflow: workflow.map(str::to_string),
        kind,
        verdict: verdict.to_string(),
        reason: reason.to_string(),
        ts,
        status: if binds(verdict) {
            RulingStatus::Open
        } else {
            RulingStatus::Resolved
        },
        blocks: 0,
        override_reason: None,
        lapse_reason: None,
        refusals: 0,
    };
    with_store(state, |rulings| {
        for old in rulings.iter_mut().filter(|old| {
            old.status == RulingStatus::Open
                && old.kind == kind
                && old.session == session
                && old.workflow.as_deref() == workflow
        }) {
            old.status = RulingStatus::Resolved;
        }
        rulings.push(ruling.clone());
        ruling
    })
}

/// Every stored ruling, open or not.
pub(crate) fn all(state: &StateDir) -> Vec<Ruling> {
    load(&path(state))
}

/// Open rulings, newest last; `scope` limits them to one session.
pub fn open_rulings(state: &StateDir, scope: Option<&str>) -> Vec<Ruling> {
    load(&path(state))
        .into_iter()
        .filter(|ruling| ruling.status == RulingStatus::Open)
        .filter(|ruling| scope.is_none_or(|session| ruling.session == session))
        .collect()
}

/// The newest open ruling of `kind` for a session or a workflow.
pub(crate) fn find_open(
    state: &StateDir,
    kind: RulingKind,
    session: Option<&str>,
    workflow: Option<&str>,
) -> Option<Ruling> {
    load(&path(state)).into_iter().rev().find(|ruling| {
        ruling.status == RulingStatus::Open
            && ruling.kind == kind
            && session.is_none_or(|session| ruling.session == session)
            && workflow.is_none_or(|id| ruling.workflow.as_deref() == Some(id))
    })
}

/// Mark a ruling overridden. This is the operator's lever: the CLI refuses it inside an agent
/// session, and the dashboard calls it for its `o` key.
pub fn override_ruling(state: &StateDir, id: &str, reason: Option<&str>) -> CtxResult<Ruling> {
    let reason = reason.map(|text| {
        crate::utils::truncate_bytes(super::super::snapshot::redact_text(text), Some(512))
    });
    let outcome = with_store(state, |rulings| {
        let ruling = rulings.iter_mut().find(|ruling| ruling.id == id)?;
        if ruling.status != RulingStatus::Open {
            return Some(Err(format!("ruling {id} is not open")));
        }
        ruling.status = RulingStatus::Overridden;
        ruling.override_reason = reason;
        Some(Ok(ruling.clone()))
    });
    match outcome {
        Some(Some(done)) => Ok(done?),
        Some(None) => Err(format!("no ruling with id {id}").into()),
        None => Err("could not update the ruling store".into()),
    }
}

/// Count one workflow-gate refusal against a ruling.
pub(crate) fn note_refusal(state: &StateDir, id: &str) {
    let _ = with_store(state, |rulings| {
        if let Some(ruling) = rulings.iter_mut().find(|ruling| ruling.id == id) {
            ruling.refusals += 1;
        }
    });
}

/// Stop an open ruling of `kind` binding because its superseding consult cannot run, once `ready`
/// says it has had its chances. A `workflow` of `None` matches any. Returns the lapsed rulings;
/// nothing is written when none qualify.
pub(crate) fn lapse_open(
    state: &StateDir,
    kind: RulingKind,
    session: &str,
    workflow: Option<&str>,
    reason: &str,
    ready: &dyn Fn(&Ruling) -> bool,
) -> Vec<Ruling> {
    let qualifies = |ruling: &Ruling| {
        ruling.status == RulingStatus::Open
            && ruling.kind == kind
            && ruling.session == session
            && workflow.is_none_or(|id| ruling.workflow.as_deref() == Some(id))
            && ready(ruling)
    };
    if !load(&path(state)).iter().any(qualifies) {
        return Vec::new();
    }
    with_store(state, |rulings| {
        rulings
            .iter_mut()
            .filter(|ruling| qualifies(ruling))
            .map(|ruling| {
                ruling.status = RulingStatus::Lapsed;
                ruling.lapse_reason = Some(reason.to_string());
                ruling.clone()
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Resolve a session's open rulings of one kind, e.g. a retry stop once the streak ends.
pub(crate) fn resolve_open(state: &StateDir, kind: RulingKind, session: &str) {
    let _ = with_store(state, |rulings| {
        for ruling in rulings.iter_mut().filter(|ruling| {
            ruling.status == RulingStatus::Open && ruling.kind == kind && ruling.session == session
        }) {
            ruling.status = RulingStatus::Resolved;
        }
    });
}

/// The reason to block this Stop with, when an open `not_done` ruling still has blocks left.
/// Counts the block under the store lock so concurrent hooks cannot exceed the cap.
pub(crate) fn take_stop_block(state: &StateDir, session: &str) -> Option<String> {
    with_store(state, |rulings| {
        let ruling = rulings.iter_mut().rev().find(|ruling| {
            ruling.status == RulingStatus::Open
                && ruling.kind == RulingKind::Done
                && ruling.session == session
                && ruling.blocks < MAX_STOP_BLOCKS
        })?;
        ruling.blocks += 1;
        Some(format!(
            "Supervisor ruling {} (not done): {} The operator can lift this with `zirv ctx \
             supervisor override {}`.",
            ruling.id, ruling.reason, ruling.id
        ))
    })
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<String> {
        vec!["queue".to_string(), "table".to_string()]
    }

    #[test]
    fn each_kind_parses_only_its_strict_format() {
        let parse = |kind, text: &str| parse_reply(kind, text, &opts());
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(parse(RulingKind::Plan, "APPROVE"), pair("approve", ""));
        assert_eq!(
            parse(RulingKind::Plan, "REVISE: add a rollback"),
            pair("revise", "add a rollback")
        );
        assert_eq!(
            parse(RulingKind::Plan, " REVISE: a\nmore detail "),
            pair("revise", "a\nmore detail")
        );
        assert_eq!(parse(RulingKind::Done, "DONE"), pair("done", ""));
        assert_eq!(
            parse(RulingKind::Done, "NOT_DONE: no tests"),
            pair("not_done", "no tests")
        );
        assert_eq!(parse(RulingKind::Retry, "RETRY"), pair("retry", ""));
        assert_eq!(
            parse(RulingKind::Retry, "STOP: needs a login"),
            pair("stop", "needs a login")
        );
        assert_eq!(parse(RulingKind::Choice, "CHOICE: 1"), pair("queue", ""));
        assert_eq!(
            parse(RulingKind::Choice, "CHOICE: 2\nREASON: simpler"),
            pair("table", "simpler")
        );
    }

    #[test]
    fn an_unparseable_reply_yields_no_ruling() {
        let parse = |kind, text: &str| parse_reply(kind, text, &opts());
        for (kind, text) in [
            (RulingKind::Plan, ""),
            (RulingKind::Plan, "NO_ADVICE"),
            (RulingKind::Plan, "approve"),
            (RulingKind::Plan, "APPROVED"),
            (RulingKind::Plan, "REVISE"),
            (RulingKind::Plan, "REVISE:   "),
            (RulingKind::Plan, "Looks fine. APPROVE"),
            (RulingKind::Plan, "NOT_DONE: wrong kind"),
            (RulingKind::Done, "REVISE: wrong kind"),
            (RulingKind::Retry, "STOP"),
            (RulingKind::Choice, "CHOICE: 0"),
            (RulingKind::Choice, "CHOICE: 3"),
            (RulingKind::Choice, "CHOICE: queue"),
            (RulingKind::Choice, "I pick the table"),
        ] {
            assert_eq!(parse(kind, text), None, "{kind:?} {text:?}");
        }
    }

    #[test]
    fn a_new_ruling_supersedes_the_open_one_of_its_kind_and_scope_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let first =
            record(&state, "s1", Some("wf"), RulingKind::Plan, "revise", "a").expect("first");
        let other =
            record(&state, "s1", Some("wf2"), RulingKind::Plan, "revise", "b").expect("other");
        let second =
            record(&state, "s1", Some("wf"), RulingKind::Plan, "revise", "c").expect("second");
        let open: Vec<String> = open_rulings(&state, None)
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(open, vec![other.id, second.id]);
        assert!(!open.contains(&first.id));
        let approve =
            record(&state, "s1", Some("wf"), RulingKind::Plan, "approve", "").expect("approve");
        assert_eq!(
            approve.status,
            RulingStatus::Resolved,
            "approvals bind nothing"
        );
        assert!(open_rulings(&state, Some("s2")).is_empty());
    }
}
