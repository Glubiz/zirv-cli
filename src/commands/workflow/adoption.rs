//! Pure workflow-adoption detector: whether a session has done "substantial"
//! edit work with no active `zirv workflow`, and what to say about it.
//!
//! Mirrors `rot::signals` (`src/commands/ctx/rot.rs:130-168`): every function
//! here is pure -- no fs/clock/env/net -- so identical events always produce
//! identical signals/decisions. Callers (`ctx::hook`) own persistence and
//! delivery; this module only counts and decides.
//!
//! The same transcript scan also counts skill loads for the sibling "no skill
//! loaded" nudge (`skill_nudge_due`/`skill_nudge_text`). Issue #539's own
//! telemetry (`TelemetryKind::SkillActivated`) carries no session identity
//! anything here could compare against, so this counts loads the SAME way
//! `edit_like_calls` already does: from `NormalizedEvent::ToolCall`'s own
//! `name` field, which every adapter's `parse_events` populates. That field is
//! the tool's NAME only -- never a shell command's argument text
//! (`NormalizedEvent::ToolCall` carries no command-text field at all, only
//! `name`/`input_hash`/`at_ms`, confirmed against every adapter's own
//! `parse_events`) -- so a shell-invoked `zirv skill load <id>` (a
//! `Bash`/`PowerShell` tool call whose NAME never says "skill load") is NOT
//! countable here. Only the native/MCP `skill_load` TOOL, matched by name,
//! counts; see `is_skill_load_tool`'s own doc comment.

use crate::commands::ctx::event::NormalizedEvent;

/// Edit-like tool names, matched case-insensitively. `apply_patch` is codex's
/// own edit tool; the rest are claude's.
const EDIT_LIKE_TOOLS: &[&str] = &["edit", "write", "multiedit", "notebookedit", "apply_patch"];

/// Edit-call count at or above which work counts as substantial on its own.
///
/// Wrapper behaviour redesign (2026-09-01): raised from 5 to 12. The prior
/// threshold fired the nudge -- the only steering text zirv ever types into a
/// live session -- on ordinary bounded work well short of "substantial",
/// pushing toward more process regardless of diff size. See
/// `docs/superpowers/specs/2026-09-01-wrapper-behaviour-redesign.md`.
pub const SUBSTANTIAL_EDIT_CALLS: usize = 12;
/// Turn count above which even a single edit call counts as substantial.
///
/// Wrapper behaviour redesign (2026-09-01): raised from 12 to 25, alongside
/// [`SUBSTANTIAL_EDIT_CALLS`], for the same proportionality reason.
pub const SUBSTANTIAL_TURNS: usize = 25;
/// Minimum turn gap between one nudge and the next. Shared by the workflow
/// nudge (`nudge_due`) and the skill nudge (`skill_nudge_due`) -- each keeps
/// its own `last_*_nudged_turn`, so the two cadences never suppress each
/// other, but the gap itself is the identical constant.
pub const NUDGE_EVERY_TURNS: usize = 5;

/// Adoption-relevant counts over a session's events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdoptionSignals {
    pub edit_like_calls: usize,
    pub turns: usize,
    /// Native/MCP `skill_load` tool calls counted the same way
    /// `edit_like_calls` is -- see this module's own doc comment for why a
    /// shell-invoked `zirv skill load` is not (and cannot be) counted here.
    pub skill_loads: usize,
}

/// Whether `name` is a skill-load tool call: either the bare native/MCP tool
/// name `skill_load`, or a host-namespaced variant of it such as
/// `mcp__zirv__skill_load` -- matched by suffix, case-insensitively, the same
/// tolerance `EDIT_LIKE_TOOLS` matching already has for a tool's own casing.
/// Never matches a shell tool (`Bash`/`PowerShell`): those carry no argument
/// text in `NormalizedEvent::ToolCall` for this function to look at, only a
/// `name` of `"Bash"`/`"PowerShell"` itself -- see this module's own doc
/// comment.
fn is_skill_load_tool(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with("skill_load")
}

/// Counts edit-like tool calls, skill-load tool calls, and turns across
/// `events`. Pure and cheap enough to run on every Stop hook invocation.
pub fn signals(events: &[NormalizedEvent]) -> AdoptionSignals {
    let mut edit_like_calls = 0usize;
    let mut turns = 0usize;
    let mut skill_loads = 0usize;
    for event in events {
        match event {
            NormalizedEvent::TurnStart { .. } => turns += 1,
            NormalizedEvent::ToolCall { name, .. }
                if EDIT_LIKE_TOOLS
                    .iter()
                    .any(|tool| name.eq_ignore_ascii_case(tool)) =>
            {
                edit_like_calls += 1;
            }
            NormalizedEvent::ToolCall { name, .. } if is_skill_load_tool(name) => {
                skill_loads += 1;
            }
            _ => {}
        }
    }
    AdoptionSignals {
        edit_like_calls,
        turns,
        skill_loads,
    }
}

/// Whether `s` describes "substantial" work: enough edit calls on its own, or
/// a long enough session that has done at least one edit.
pub fn is_substantial(s: &AdoptionSignals) -> bool {
    s.edit_like_calls >= SUBSTANTIAL_EDIT_CALLS
        || (s.turns >= SUBSTANTIAL_TURNS && s.edit_like_calls >= 1)
}

/// Operator-controlled strictness for workflow adoption, ordered
/// `Off < Advise < Nudge < Enforce`. Modeled on `deploy::DeployTier`
/// (`src/commands/workflow/deploy.rs:17-26`).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum AdoptionPolicy {
    Off,
    Advise,
    #[default]
    Nudge,
    Enforce,
}

impl std::fmt::Display for AdoptionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Advise => "advise",
            Self::Nudge => "nudge",
            Self::Enforce => "enforce",
        })
    }
}

/// Whether a nudge should fire right now: never below `Nudge`, never while a
/// workflow is already active, never for non-substantial work. The first
/// nudge fires immediately once substantial; after that, only every
/// [`NUDGE_EVERY_TURNS`] turns.
pub fn nudge_due(
    policy: AdoptionPolicy,
    substantial: bool,
    workflow_active: bool,
    turns_now: usize,
    last_nudged_turn: Option<usize>,
) -> bool {
    if policy < AdoptionPolicy::Nudge || !substantial || workflow_active {
        return false;
    }
    match last_nudged_turn {
        None => true,
        Some(last) => turns_now >= last + NUDGE_EVERY_TURNS,
    }
}

/// The nudge message itself. There is no task text at the point a nudge
/// fires, so it never guesses a workflow kind: the printed command omits
/// the id entirely -- `zirv workflow start` with no id selects
/// deterministically from the task once the operator supplies one. Under
/// [`AdoptionPolicy::Enforce`] an extra sentence names the delegation gate
/// this policy also applies (`ctx::agent::run_with`).
///
/// Wrapper behaviour redesign (2026-09-01): the wording is now proportional
/// -- it names a workflow as something to start "if it spans several areas
/// or carries real risk" and says outright that a bounded change may finish
/// without one, rather than unconditionally telling the session to start one
/// now. See `docs/superpowers/specs/2026-09-01-wrapper-behaviour-redesign.md`.
pub fn nudge_text(signals: &AdoptionSignals, policy: AdoptionPolicy) -> String {
    let mut text = format!(
        "[zirv workflow] this has grown into substantial work ({} edit calls over {} turns) \
         with no active zirv workflow. If it spans several areas or carries real risk, start \
         one now: zirv workflow start --task \"<summary>\". A bounded change may finish \
         without one.",
        signals.edit_like_calls, signals.turns
    );
    if policy == AdoptionPolicy::Enforce {
        // ASCII double-hyphen, not a real em dash: this text rides the Stop
        // hook's `systemMessage` and `UserPromptSubmit`'s `additionalContext`,
        // both held to the same "no em dashes in user-facing copy" rule every
        // other hook-adjacent string in this crate is tested against (see
        // e.g. `hook.rs`'s `an_advisory_verdict_prints_a_non_blocking_
        // system_message`).
        text.push_str(
            " -- workflow.adoption = enforce: zirv agent delegation is held until a workflow is \
             active.",
        );
    }
    text
}

/// Whether the skill nudge should fire right now -- substantial work, zero
/// skill loads this session, and due by the same [`NUDGE_EVERY_TURNS`]
/// cadence [`nudge_due`] uses, but its OWN `last_skill_nudged_turn` -- so
/// this nudge and the workflow-adoption nudge above never suppress or get
/// suppressed by each other.
///
/// Deliberately takes no `AdoptionPolicy`/`workflow_active`, unlike
/// `nudge_due`: an operator's workflow-adoption strictness governs the
/// WORKFLOW nudge, not whether the skill library gets pointed at, and a
/// workflow being active says nothing about whether a skill was ever loaded
/// -- skills matter inside a workflow too. The caller (`ctx::hook`) still
/// gates the whole feature on `prompt.skill_index`, and inherits whatever
/// gate stops the transcript scan that produces `skill_loads`/`substantial`
/// in the first place (see `ctx::hook::adoption_stop_nudge`'s own doc
/// comment on the `workflow.adoption == Off` case) -- neither of which
/// belongs in this pure function's own signature.
pub fn skill_nudge_due(
    substantial: bool,
    skill_loads: usize,
    turns_now: usize,
    last_skill_nudged_turn: Option<usize>,
) -> bool {
    if !substantial || skill_loads > 0 {
        return false;
    }
    match last_skill_nudged_turn {
        None => true,
        Some(last) => turns_now >= last + NUDGE_EVERY_TURNS,
    }
}

/// The skill-nudge message itself. ASCII only, no em dashes -- the same
/// hook-adjacent-copy rule [`nudge_text`] follows. Names only
/// `edit_like_calls`/`turns`: `skill_loads` is always zero whenever this
/// fires ([`skill_nudge_due`]'s own gate), so it has nothing to add.
pub fn skill_nudge_text(signals: &AdoptionSignals) -> String {
    format!(
        "[zirv skills] substantial work ({} edit calls over {} turns) and no zirv skill loaded \
         this session. Check the skill index: zirv skill list --match \"<task>\" then zirv \
         skill load <id>.",
        signals.edit_like_calls, signals.turns
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> NormalizedEvent {
        NormalizedEvent::ToolCall {
            name: name.to_string(),
            input_hash: 0,
            at_ms: None,
        }
    }

    #[test]
    fn signals_count_edit_like_tools_case_insensitively() {
        let events = vec![
            tool("Edit"),
            tool("WRITE"),
            tool("multiEdit"),
            tool("NotebookEdit"),
            tool("apply_patch"),
            tool("Bash"),
            tool("Read"),
        ];
        let s = signals(&events);
        assert_eq!(s.edit_like_calls, 5);
        assert_eq!(s.turns, 0);
        assert_eq!(s.skill_loads, 0);
    }

    #[test]
    fn signals_ignore_non_tool_events_and_count_turns() {
        let events = vec![
            NormalizedEvent::TurnStart { at_ms: None },
            tool("Edit"),
            NormalizedEvent::AssistantFinal {
                text: String::new(),
                input_tokens: 0,
                at_ms: None,
            },
            NormalizedEvent::ToolResult { is_error: false },
            NormalizedEvent::Compaction,
            NormalizedEvent::TurnStart { at_ms: None },
        ];
        let s = signals(&events);
        assert_eq!(s.edit_like_calls, 1);
        assert_eq!(s.turns, 2);
        assert_eq!(s.skill_loads, 0);
    }

    /// A native/MCP `skill_load` tool call counts, a host-namespaced variant
    /// of it counts, and an unrelated `zirv skill list` (a different tool
    /// name entirely, not a load) does not.
    #[test]
    fn signals_counts_a_skill_load_tool_call_and_ignores_skill_list() {
        let events = vec![
            tool("skill_load"),
            tool("mcp__zirv__skill_load"),
            tool("skill_list"),
        ];
        let s = signals(&events);
        assert_eq!(s.skill_loads, 2, "{s:?}");
    }

    /// `NormalizedEvent::ToolCall` carries only a tool NAME, never a shell
    /// command's own argument text, so a shell-invoked `zirv skill load x` is
    /// invisible to this scan -- `Bash`/`PowerShell` never counts no matter
    /// what a real session would have typed into that shell. The CLI itself
    /// now closes the gap from the other side (`ctx::hook::
    /// record_shell_skill_load`, called from `skill::run_load` after a
    /// successful load, bumping `AdoptionRecord::shell_skill_loads` directly
    /// rather than through this transcript scan at all).
    #[test]
    fn signals_cannot_see_a_shell_invoked_skill_load_only_the_tool_name() {
        let events = vec![tool("Bash"), tool("PowerShell")];
        let s = signals(&events);
        assert_eq!(
            s.skill_loads, 0,
            "a shell tool call's own command text is not modeled at all: {s:?}"
        );
    }

    #[test]
    fn substantial_by_edit_count_alone() {
        let s = AdoptionSignals {
            edit_like_calls: SUBSTANTIAL_EDIT_CALLS,
            turns: 1,
            skill_loads: 0,
        };
        assert!(is_substantial(&s));
        let below = AdoptionSignals {
            edit_like_calls: SUBSTANTIAL_EDIT_CALLS - 1,
            turns: 1,
            skill_loads: 0,
        };
        assert!(!is_substantial(&below));
    }

    #[test]
    fn substantial_by_turns_needs_at_least_one_edit() {
        let s = AdoptionSignals {
            edit_like_calls: 1,
            turns: SUBSTANTIAL_TURNS,
            skill_loads: 0,
        };
        assert!(is_substantial(&s));

        let no_edits = AdoptionSignals {
            edit_like_calls: 0,
            turns: SUBSTANTIAL_TURNS + 10,
            skill_loads: 0,
        };
        assert!(
            !is_substantial(&no_edits),
            "turns alone, with no edits, is not substantial"
        );

        let below_turns = AdoptionSignals {
            edit_like_calls: 1,
            turns: SUBSTANTIAL_TURNS - 1,
            skill_loads: 0,
        };
        assert!(!is_substantial(&below_turns));
    }

    #[test]
    fn adoption_policy_orders_by_strictness() {
        assert!(AdoptionPolicy::Off < AdoptionPolicy::Advise);
        assert!(AdoptionPolicy::Advise < AdoptionPolicy::Nudge);
        assert!(AdoptionPolicy::Nudge < AdoptionPolicy::Enforce);
        assert_eq!(AdoptionPolicy::default(), AdoptionPolicy::Nudge);
    }

    #[test]
    fn nudge_due_requires_at_least_nudge_policy() {
        assert!(!nudge_due(AdoptionPolicy::Off, true, false, 20, None));
        assert!(!nudge_due(AdoptionPolicy::Advise, true, false, 20, None));
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 20, None));
        assert!(nudge_due(AdoptionPolicy::Enforce, true, false, 20, None));
    }

    #[test]
    fn nudge_due_requires_substantial_and_no_active_workflow() {
        assert!(!nudge_due(AdoptionPolicy::Nudge, false, false, 20, None));
        assert!(!nudge_due(AdoptionPolicy::Nudge, true, true, 20, None));
    }

    #[test]
    fn nudge_due_fires_immediately_then_every_nudge_every_turns() {
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 12, None));

        // Just nudged at turn 12: not due again until turn 17.
        assert!(!nudge_due(AdoptionPolicy::Nudge, true, false, 16, Some(12)));
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 17, Some(12)));
        assert!(nudge_due(AdoptionPolicy::Nudge, true, false, 25, Some(12)));
    }

    /// The nudge names no kind (there is no task text yet to classify one
    /// from) -- just the counts and an id-less `zirv workflow start`, which
    /// selects deterministically once the operator supplies `--task`.
    #[test]
    fn nudge_text_names_no_kind_and_reports_the_counts() {
        let s = AdoptionSignals {
            edit_like_calls: 7,
            turns: 9,
            skill_loads: 0,
        };
        let text = nudge_text(&s, AdoptionPolicy::Nudge);
        assert!(text.contains("7 edit calls over 9 turns"), "{text}");
        assert!(text.contains("zirv workflow start --task"), "{text}");
        assert!(!text.contains("enforce"), "{text}");
    }

    #[test]
    fn nudge_does_not_fire_at_old_thresholds_but_does_at_new_ones() {
        // Old thresholds (5 edits / 12 turns) no longer count as substantial.
        let old = AdoptionSignals {
            edit_like_calls: 5,
            turns: 12,
            skill_loads: 0,
        };
        assert!(!is_substantial(&old));
        assert!(!nudge_due(
            AdoptionPolicy::Nudge,
            is_substantial(&old),
            false,
            12,
            None
        ));

        // New thresholds (12 edits / 25 turns) do.
        let new_by_edits = AdoptionSignals {
            edit_like_calls: SUBSTANTIAL_EDIT_CALLS,
            turns: 1,
            skill_loads: 0,
        };
        assert!(is_substantial(&new_by_edits));
        let new_by_turns = AdoptionSignals {
            edit_like_calls: 1,
            turns: SUBSTANTIAL_TURNS,
            skill_loads: 0,
        };
        assert!(is_substantial(&new_by_turns));
    }

    #[test]
    fn nudge_text_under_enforce_names_the_delegation_gate() {
        let s = AdoptionSignals {
            edit_like_calls: 5,
            turns: 5,
            skill_loads: 0,
        };
        let text = nudge_text(&s, AdoptionPolicy::Enforce);
        assert!(text.contains("workflow.adoption = enforce"), "{text}");
        assert!(text.contains("zirv agent delegation is held"), "{text}");
    }

    // -- skill_nudge_due / skill_nudge_text --------------------------------

    #[test]
    fn skill_nudge_due_requires_substantial_and_zero_loads() {
        assert!(!skill_nudge_due(false, 0, 20, None), "not substantial");
        assert!(!skill_nudge_due(true, 1, 20, None), "already loaded one");
        assert!(skill_nudge_due(true, 0, 20, None));
    }

    #[test]
    fn skill_nudge_due_has_its_own_cadence_independent_of_the_workflow_nudge() {
        // Fires immediately once substantial with no loads.
        assert!(skill_nudge_due(true, 0, 12, None));
        // Not due again until NUDGE_EVERY_TURNS later.
        assert!(!skill_nudge_due(true, 0, 16, Some(12)));
        assert!(skill_nudge_due(true, 0, 17, Some(12)));

        // Its own cadence field, `last_skill_nudged_turn`, is independent of
        // whatever turn the WORKFLOW nudge last fired at: a workflow nudge at
        // turn 12 must not silence (or force) a skill nudge whose own last
        // fire was, say, turn 5.
        assert!(!skill_nudge_due(true, 0, 9, Some(5)));
        assert!(skill_nudge_due(true, 0, 10, Some(5)));
    }

    #[test]
    fn skill_nudge_text_names_edit_calls_and_turns_but_no_specific_skill() {
        let s = AdoptionSignals {
            edit_like_calls: 14,
            turns: 20,
            skill_loads: 0,
        };
        let text = skill_nudge_text(&s);
        assert!(text.contains("14 edit calls over 20 turns"), "{text}");
        assert!(text.contains("zirv skill list --match"), "{text}");
        assert!(text.contains("zirv skill load <id>"), "{text}");
        assert!(text.is_ascii(), "{text}");
        assert!(!text.contains('\u{2014}'), "no real em dash: {text}");
    }
}
