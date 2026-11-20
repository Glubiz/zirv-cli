//! Pure rules for the approvals bridge (#950 slice 6): which requests may be
//! held for the operator, what the operator is shown, and which wire value a
//! decision becomes. Nothing here touches I/O; `turn` owns every write.
//!
//! The wire has exactly two answers, `accept` (once) and `decline`. Session
//! scopes, execpolicy and network amendments, and `cancel` are never produced:
//! they widen what codex auto-approves later, outside zirv's view.

use std::time::Duration;

use serde_json::{Value, json};

use super::codec::RequestId;
use crate::commands::ctx::approvals::Decision;
use crate::commands::ctx::exec::launch::ApprovalBridge;

/// Fixed dashboard label: the worker never chooses it.
pub const TOOL: &str = "codex command";

/// A command approval request whose ids match the running turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandAsk {
    pub item_id: String,
    pub approval_id: Option<String>,
    pub command: String,
    pub cwd: Option<String>,
    pub reason: Option<String>,
}

/// How a request ends. Only `Accept` lets the command run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// The operator said no.
    Deny,
    /// Nobody said yes; the reason is one of the logged words.
    Decline(&'static str),
}

/// `params` of `item/commandExecution/requestApproval`, or why it is refused.
pub fn parse_command(
    params: &Value,
    thread: &str,
    turn: Option<&str>,
) -> Result<CommandAsk, &'static str> {
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    if text("threadId") != Some(thread) {
        return Err("threadId is not the running thread");
    }
    if turn.is_none() || text("turnId") != turn {
        return Err("turnId is not the running turn");
    }
    let item_id = text("itemId")
        .filter(|id| !id.is_empty())
        .ok_or("no itemId")?;
    // Without the command text the operator could only approve blind.
    let command = text("command")
        .filter(|command| !command.trim().is_empty())
        .ok_or("no command to show")?;
    Ok(CommandAsk {
        item_id: item_id.to_string(),
        approval_id: text("approvalId").map(str::to_string),
        command: command.to_string(),
        cwd: text("cwd").map(str::to_string),
        reason: text("reason").map(str::to_string),
    })
}

/// Mixed into the dashboard's request id, so two identical commands (one
/// `itemId` can carry several `approvalId`s) never share a record.
pub fn distinct(id: &RequestId, ask: &CommandAsk) -> String {
    format!(
        "{}\0{}\0{}",
        show(id),
        ask.item_id,
        ask.approval_id.as_deref().unwrap_or_default()
    )
}

/// A JSON-RPC id as a log word.
pub fn show(id: &RequestId) -> String {
    match id {
        RequestId::Num(n) => n.to_string(),
        RequestId::Str(s) => s.clone(),
    }
}

/// What the operator's answer, or its absence, means. `waited` is how long
/// the hold ran; `hold_for_dashboard` reports every failure as `None`.
pub fn decide(decision: Option<Decision>, waited: Duration, hold: Duration) -> Verdict {
    match decision {
        // `AllowAlways` is never offered; if one arrives it is still just once.
        Some(Decision::Allow | Decision::AllowAlways) => Verdict::Accept,
        Some(Decision::Deny) => Verdict::Deny,
        // Release means "show the native dialog", and a headless worker has none.
        Some(Decision::Release) => Verdict::Decline("policy"),
        None if waited >= hold => Verdict::Decline("timeout"),
        None => Verdict::Decline("no-dashboard"),
    }
}

pub fn wire(verdict: Verdict) -> Value {
    let decision = if verdict == Verdict::Accept {
        "accept"
    } else {
        "decline"
    };
    json!({"decision": decision})
}

/// The thread's approval policy once the bridge is in play. The bridge turns
/// `never` into `on-request` unless the caller chose a policy; an explicit
/// `-a never` also turns the bridge off for the run. The returned bridge is
/// the one that stays active.
pub fn seed_policy(
    thread: &mut Value,
    explicit: bool,
    bridge: Option<ApprovalBridge>,
) -> Option<ApprovalBridge> {
    if bridge.is_some()
        && !explicit
        && let Some(thread) = thread.as_object_mut()
    {
        thread.insert("approvalPolicy".into(), "on-request".into());
    }
    bridge.filter(|_| thread.get("approvalPolicy").and_then(Value::as_str) != Some("never"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Value {
        json!({"threadId": "thr-1", "turnId": "turn-1", "itemId": "item-1", "startedAtMs": 1,
            "command": "cargo test", "cwd": "/repo", "reason": "needs the network"})
    }

    fn bridge() -> Option<ApprovalBridge> {
        Some(ApprovalBridge {
            parent: Some("abc123".into()),
            hold: Duration::from_secs(300),
        })
    }

    #[test]
    fn a_matching_command_request_is_parsed_and_a_mismatch_is_refused() {
        let ask = parse_command(&params(), "thr-1", Some("turn-1")).expect("parses");
        assert_eq!(ask.command, "cargo test");
        assert_eq!(ask.cwd.as_deref(), Some("/repo"));
        assert_eq!(ask.approval_id, None);
        for (bad, why) in [
            (json!({"threadId": "other"}), "thread"),
            (json!({"turnId": "other"}), "turn"),
            (json!({"turnId": null}), "turn"),
            (json!({"itemId": null}), "itemId"),
            (json!({"itemId": ""}), "itemId"),
            (json!({"command": null}), "command"),
            (json!({"command": "  "}), "command"),
        ] {
            let mut p = params();
            p.as_object_mut()
                .expect("object")
                .extend(bad.as_object().expect("object").clone());
            let err = parse_command(&p, "thr-1", Some("turn-1")).expect_err(why);
            assert!(err.contains(why), "{err}");
        }
        assert!(
            parse_command(&params(), "thr-1", None).is_err(),
            "no running turn id yet"
        );
        assert!(parse_command(&json!(null), "thr-1", Some("turn-1")).is_err());
    }

    #[test]
    fn two_identical_commands_get_distinct_keys() {
        let ask = parse_command(&params(), "thr-1", Some("turn-1")).expect("parses");
        let mut other = ask.clone();
        other.approval_id = Some("a-2".into());
        let key = |id: i64, ask: &CommandAsk| distinct(&RequestId::Num(id), ask);
        assert_ne!(key(1, &ask), key(2, &ask), "the JSON-RPC id");
        assert_ne!(key(1, &ask), key(1, &other), "the approvalId");
    }

    #[test]
    fn only_an_allow_accepts_and_the_wire_never_carries_a_wider_scope() {
        let hold = Duration::from_secs(300);
        let quick = Duration::from_secs(1);
        for (decision, verdict) in [
            (Some(Decision::Allow), Verdict::Accept),
            (Some(Decision::AllowAlways), Verdict::Accept),
            (Some(Decision::Deny), Verdict::Deny),
            (Some(Decision::Release), Verdict::Decline("policy")),
            (None, Verdict::Decline("no-dashboard")),
        ] {
            assert_eq!(decide(decision, quick, hold), verdict, "{decision:?}");
        }
        assert_eq!(decide(None, hold, hold), Verdict::Decline("timeout"));
        assert_eq!(wire(Verdict::Accept), json!({"decision": "accept"}));
        for verdict in [Verdict::Deny, Verdict::Decline("error")] {
            assert_eq!(wire(verdict), json!({"decision": "decline"}));
        }
    }

    #[test]
    fn the_policy_is_on_request_unless_the_caller_chose_one_and_never_turns_the_bridge_off() {
        let seeded = || json!({"approvalPolicy": "never", "cwd": "/repo"});
        let mut thread = seeded();
        assert!(seed_policy(&mut thread, false, bridge()).is_some());
        assert_eq!(thread["approvalPolicy"], "on-request");
        assert_eq!(thread["cwd"], "/repo", "nothing else changes");
        let mut thread = seeded();
        assert!(
            seed_policy(&mut thread, true, bridge()).is_none(),
            "-a never"
        );
        assert_eq!(thread["approvalPolicy"], "never");
        let mut thread = json!({"approvalPolicy": "untrusted"});
        assert!(seed_policy(&mut thread, true, bridge()).is_some());
        assert_eq!(
            thread["approvalPolicy"], "untrusted",
            "an explicit policy wins"
        );
        let mut thread = seeded();
        assert!(seed_policy(&mut thread, false, None).is_none());
        assert_eq!(
            thread["approvalPolicy"], "never",
            "bridge off keeps today's seed"
        );
    }
}
