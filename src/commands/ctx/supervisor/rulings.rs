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

/// Helpers drift into markdown (`**Choice: 2 — why.**`): tolerate emphasis marks and case.
fn is_mark(c: char) -> bool {
    matches!(c, '*' | '_' | '`')
}

/// What follows a case-insensitive `label` at the start of `head`, without leading marks/spaces.
fn after_label<'a>(head: &'a str, label: &str) -> Option<&'a str> {
    let (start, rest) = (head.get(..label.len())?, head.get(label.len()..)?);
    start
        .eq_ignore_ascii_case(label)
        .then(|| rest.trim_start_matches(|c: char| is_mark(c) || c == ' '))
}

/// One line as a choice verdict: `CHOICE: 2`, `Ruling: choice 2`, `Ruling: 2` or a letter
/// (`Choice: B.`), with emphasis marks tolerated. Returns the 1-based option number and the text
/// after it; prose after the number is fine, but a second candidate (`1 or 2`, `1/2`) is not.
fn choice_head(line: &str) -> Option<(usize, &str)> {
    let line = line.trim().trim_matches(is_mark);
    let ruled = after_label(line, "RULING:");
    let line = ruled.unwrap_or(line);
    let after = after_label(line, "CHOICE:")
        .or_else(|| ruled.and_then(|_| after_label(line, "CHOICE ")))
        .or(ruled)?;
    let digits = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    let (number, tail) = match after[..digits].parse::<usize>() {
        Ok(number) => (number, &after[digits..]),
        Err(_) => {
            let letter = after.chars().next().filter(char::is_ascii_uppercase)?;
            let tail = &after[1..];
            // `Choice: A simpler store` is prose, not option A.
            let separated = tail
                .trim_start()
                .starts_with(['.', ':', ')', '-', '—', '–', '*', '_', '`']);
            if !tail.is_empty() && !separated {
                return None;
            }
            ((letter as u8 - b'A') as usize + 1, tail)
        }
    };
    if tail.chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let next = tail.trim_start();
    let lower = next.to_ascii_lowercase();
    let second = lower
        .strip_prefix("or ")
        .or_else(|| lower.strip_prefix("and "))
        .and_then(|rest| rest.chars().next());
    if next.starts_with('/') || second.is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((number, tail))
}

/// A reply's first line as an offered option's pick: it opens with the option's own text (case
/// and emphasis marks aside), then ends or continues after a separator (`:` `.` `,` ` -`), and
/// names no other offered option. Returns the 1-based number and the text after the label.
/// `Queue is slower; table.` is prose, not a pick of `queue`.
fn option_label<'a>(line: &'a str, options: &[String]) -> Option<(usize, &'a str)> {
    let line = line.trim().trim_start_matches(is_mark);
    let lower = line.to_ascii_lowercase();
    let (at, rest) = options
        .iter()
        .enumerate()
        .filter(|(_, option)| !option.trim().is_empty())
        .filter_map(|(at, option)| {
            let label = option.trim();
            let (start, rest) = (line.get(..label.len())?, line.get(label.len()..)?);
            let after = rest.trim_start_matches(is_mark);
            let separated = after.is_empty()
                || after.starts_with([':', '.', ','])
                || ["-", "—", "–"]
                    .iter()
                    .any(|dash| after.trim_start().starts_with(dash) && after.starts_with(' '));
            (start.eq_ignore_ascii_case(label) && separated).then_some((at, rest))
        })
        .max_by_key(|(at, _)| options[*at].len())?;
    let names_another = options.iter().enumerate().any(|(other, option)| {
        other != at
            && !option.trim().is_empty()
            && lower.contains(&option.trim().to_ascii_lowercase())
    });
    (!names_another).then_some((at + 1, rest))
}

/// Parse one kind's strict reply into `(verdict, reason)`. Anything unparseable is `None`.
/// For a choice the verdict is the chosen option's own text.
pub(crate) fn parse_reply(
    kind: RulingKind,
    reply: &str,
    options: &[String],
) -> Option<(String, String)> {
    let reply = reply.trim();
    // A Claude helper's turn opens with zirv's own health marker, on its own line or ahead of the verdict.
    let reply = reply
        .strip_prefix("[zirv]")
        .map_or(reply, |rest| rest.trim_start());
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
        RulingKind::Done => {
            // The helper echoes the prompt's `Ruling:` label and writes the verdict as prose
            // (`Ruling: not done <why>`): accept the label, case and trailing text on the head.
            let head = head.trim_matches(is_mark);
            let head = after_label(head, "RULING:").unwrap_or(head);
            let lower = head.to_ascii_lowercase();
            let strip = |words: &[&str]| {
                words
                    .iter()
                    .find(|word| lower.starts_with(**word))
                    .map(|word| &head[word.len()..])
            };
            let (verdict, after) = match strip(&["not_done", "not done", "not-done"]) {
                Some(after) => ("not_done", after),
                None => ("done", strip(&["done"])?),
            };
            if after.chars().next().is_some_and(char::is_alphanumeric) {
                return None;
            }
            // `Done, but ...` and `Done? No, ...` hedge the verdict; they are not a bare `done`.
            if verdict == "done" && after.starts_with([',', '?']) {
                return None;
            }
            let tail = after
                .trim_start_matches(|c: char| {
                    is_mark(c) || c.is_whitespace() || ".:;,-—–".contains(c)
                })
                .trim_end_matches(is_mark);
            // `Done. But ...`, `Done - not yet` and `Done: no, ...` hedge it after punctuation.
            let first_word: String = tail
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            let hedges = [
                "but", "however", "no", "not", "except", "although", "yet", "unless",
            ];
            if verdict == "done" && hedges.contains(&first_word.as_str()) {
                return None;
            }
            let reason = [tail, rest]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (verdict == "done" || !reason.is_empty()).then(|| (verdict.to_string(), reason))
        }
        RulingKind::Retry => pair("RETRY", "retry", "STOP", "stop"),
        RulingKind::Choice => {
            // The verdict is the first line after any blank or code-fence opener (a preamble
            // line stays a rejection), and no later verdict line may name another option.
            let mut lines = reply.lines().skip_while(|line| {
                let line = line.trim();
                line.is_empty() || line.starts_with("```")
            });
            let head_line = lines.next()?;
            let (number, tail) =
                choice_head(head_line).or_else(|| option_label(head_line, options))?;
            let following: Vec<&str> = lines.collect();
            if following
                .iter()
                .filter_map(|line| choice_head(line))
                .any(|(other, _)| other != number)
            {
                return None;
            }
            let option = options.get(number.checked_sub(1)?)?;
            let following = following.join("\n");
            let following = following.trim();
            let reason = following
                .strip_prefix("REASON:")
                .unwrap_or(following)
                .trim();
            let reason = reason.strip_suffix("```").unwrap_or(reason).trim_end();
            let reason = if reason.is_empty() {
                tail.trim_start_matches(|c: char| {
                    c.is_whitespace() || is_mark(c) || ".:,-—–".contains(c)
                })
                .trim_end_matches(is_mark)
            } else {
                reason
            };
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

    /// #901 and the recorded parse-failure shapes: a reply that names one offered option, by its
    /// label or by number, parses as it; one that names two options still fails.
    #[test]
    fn a_choice_reply_naming_one_option_parses_by_label_or_number() {
        let options = vec![
            "done: verification complete".to_string(),
            "not done: name the missing evidence".to_string(),
        ];
        let parse = |text: &str| parse_reply(RulingKind::Choice, text, &options);
        assert_eq!(
            parse("done: verification complete"),
            Some((options[0].clone(), String::new()))
        );
        let (verdict, reason) =
            parse("done: verification complete. Diff inspected; all 2,297 tests passed.")
                .expect("an echoed label with rationale");
        assert_eq!(verdict, options[0]);
        assert!(reason.contains("2,297 tests passed"), "{reason}");
        let (verdict, _) = parse("**Not done: name the missing evidence.** lint was not run")
            .expect("case and emphasis marks");
        assert_eq!(verdict, options[1]);
        let (verdict, _) = parse("Ruling: 2 Flip the flag only.").expect("number after the label");
        assert_eq!(verdict, options[1]);
        let (verdict, _) = parse("Choice: A — tokens. Matches the model.").expect("letter");
        assert_eq!(verdict, options[0]);
        // A label that is only the start of a longer word is not the option.
        assert_eq!(parse("done: verification completely unverified"), None);
        // Rationale on later lines is not an option label, so it cannot reject a valid reply.
        let (verdict, _) = parse("done: verification complete\nnot done: only if lint is red")
            .expect("later lines do not count");
        assert_eq!(verdict, options[0]);
        assert_eq!(parse("Both options fit; pick whichever"), None);
    }

    /// A first word that merely equals an option is prose, not a pick; a pick is label plus
    /// separator and names no other option.
    #[test]
    fn a_leading_option_word_in_prose_is_not_a_pick() {
        let options = vec!["queue".to_string(), "table".to_string()];
        let parse = |text: &str| parse_reply(RulingKind::Choice, text, &options).map(|p| p.0);
        assert_eq!(parse("Queue is slower; table."), None);
        assert_eq!(parse("Table is wrong, queue wins"), None);
        assert_eq!(parse("queue: simpler to run").as_deref(), Some("queue"));
        assert_eq!(parse("Table.").as_deref(), Some("table"));
        assert_eq!(parse("queue - simpler").as_deref(), Some("queue"));
        assert_eq!(parse("queue: simpler than table"), None);
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

    /// #868: the real fable helper reply opened with the `[zirv]` health marker line.
    #[test]
    fn a_leading_health_marker_does_not_hide_the_verdict() {
        let parse = |text: &str| parse_reply(RulingKind::Choice, text, &opts());
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(
            parse("[zirv]\nCHOICE: 2\nREASON: read-only session"),
            pair("table", "read-only session")
        );
        assert_eq!(parse("[zirv] CHOICE: 1"), pair("queue", ""));
        assert_eq!(parse("[two words]\nCHOICE: 1"), None);
        assert_eq!(parse("[x]\nCHOICE: 1"), None);
        assert_eq!(
            parse_reply(RulingKind::Done, "[x]\nNOT_DONE: retry", &opts()),
            None
        );
        assert_eq!(parse("[zirv]\nI pick the table"), None);
    }

    /// The live 2026-10-03 failure: the helper bolded its verdict and followed the number with
    /// prose, so the strict `CHOICE: <n>` head never matched.
    #[test]
    fn a_decorated_choice_head_still_names_the_option() {
        let parse = |text: &str| parse_reply(RulingKind::Choice, text, &opts());
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(
            parse("**Choice: 2 — transcript-backed clearing.**\n\nA `tool_result` resolves it."),
            pair("table", "A `tool_result` resolves it.")
        );
        assert_eq!(parse("Choice: 1"), pair("queue", ""));
        // The live ts 1791180498 shape: mixed-case head, blank line, free prose with no REASON:.
        assert_eq!(
            parse("Choice: 1\n\nInject only the workflow bound to the composing session."),
            pair(
                "queue",
                "Inject only the workflow bound to the composing session."
            )
        );
        assert_eq!(
            parse("`CHOICE: 2`\nREASON: simpler"),
            pair("table", "simpler")
        );
        assert_eq!(
            parse("CHOICE: 2. Simpler to run."),
            pair("table", "Simpler to run.")
        );
        assert_eq!(parse("CHOICE: 2nd"), None);
    }

    /// The live 2026-10-06/07 failures (decisions.jsonl, issue #877): a letter instead of a
    /// number, the `Ruling:` label echoed in place of `CHOICE:`, and the verdict wrapped in
    /// a code fence or prose.
    #[test]
    fn a_wrapped_or_relabelled_choice_still_names_the_option() {
        let parse = |text: &str| parse_reply(RulingKind::Choice, text, &opts());
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(
            parse("Choice: A. Stamp the first page only."),
            pair("queue", "Stamp the first page only.")
        );
        assert_eq!(
            parse("**Choice: B.** It is clearer."),
            pair("table", "It is clearer.")
        );
        assert_eq!(
            parse("Ruling: 2 Flip the TTL only."),
            pair("table", "Flip the TTL only.")
        );
        assert_eq!(
            parse("Ruling: choice 2 — early-stop. It targets the loss."),
            pair("table", "early-stop. It targets the loss.")
        );
        assert_eq!(parse("```\nCHOICE: 2\n```"), pair("table", ""));
        assert_eq!(
            parse("```text\n**CHOICE: 1**\nREASON: simpler\n```"),
            pair("queue", "simpler")
        );
        assert_eq!(
            parse("CHOICE: 2\nREASON: simpler\nCHOICE: 2").map(|(verdict, _)| verdict),
            Some("table".to_string())
        );
    }

    #[test]
    fn an_ambiguous_choice_reply_still_fails() {
        let parse = |text: &str| parse_reply(RulingKind::Choice, text, &opts());
        for text in [
            "CHOICE: 1\nCHOICE: 2",
            "Choice: 1 or 2",
            "I weighed both.\n\nCHOICE: 1",
            "Choice: 1/2",
            "Choice: A simpler store wins.",
            "Choice: C",
            "Ruling: 3",
            "Ruling: use the table",
            "Here is the plan.\nNo pick.",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    /// The live 2026-10-05 failures (decisions.jsonl): the helper echoed the prompt's `Ruling:`
    /// label and wrote the verdict as prose on the same line.
    #[test]
    fn a_done_head_with_the_ruling_label_still_parses() {
        let parse = |text: &str| parse_reply(RulingKind::Done, text, &opts());
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(
            parse(
                "Ruling: done\n\nThe 12 CRM identifier additions match the documented UK brand assignments."
            ),
            pair(
                "done",
                "The 12 CRM identifier additions match the documented UK brand assignments."
            )
        );
        assert_eq!(
            parse("Ruling: done The new journal link resolves and matches its entry’s summary."),
            pair(
                "done",
                "The new journal link resolves and matches its entry’s summary."
            )
        );
        assert_eq!(
            parse(
                "Ruling: not done Required lint and test results are absent. Run `bun run lint`."
            ),
            pair(
                "not_done",
                "Required lint and test results are absent. Run `bun run lint`."
            )
        );
        assert_eq!(
            parse("**Ruling: NOT_DONE: no tests**\nrun them"),
            pair("not_done", "no tests\nrun them")
        );
        assert_eq!(parse("Ruling: not done"), None);
        assert_eq!(parse("Ruling: donefor"), None);
        assert_eq!(
            parse("Ruling: not verified. The diff alone is not enough."),
            None
        );
    }

    #[test]
    fn a_hedged_done_head_is_not_a_done_ruling() {
        let parse = |text: &str| parse_reply(RulingKind::Done, text, &opts());
        assert_eq!(parse("Done, but the tests fail."), None);
        assert_eq!(parse("Done? No, lint is missing"), None);
        assert_eq!(parse("Ruling: done, but lint is missing"), None);
        for hedged in [
            "Done. But the tests fail",
            "Done - not yet",
            "Done; however lint is missing",
            "Done: no, lint is missing",
            "Done. Except the docs",
            "DONE - Although partial",
            "Done: yet to run tests",
            "Done. Unless CI fails",
        ] {
            assert_eq!(parse(hedged), None, "{hedged}");
        }
        assert!(parse("Done. Nothing left to do").is_some());
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
