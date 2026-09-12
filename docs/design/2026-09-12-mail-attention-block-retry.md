# Mail advisory retry across an attention block

**Date:** 2026-09-12 · **Issue:** #468

## Context

`dash::mod::mail_sweep` gates both a worker pane's body delivery
(`sweep_one_pane`) and an orchestrator pane's one-line advisory
(`advise_one_pane`) on `Pane::injectable()` alone -- a turn-signal question
("has this pane's harness told zirv its turn ended, and has nothing been
typed into it since"). That question is silent about *why* a pane looks
idle: a Claude permission dialog pauses the harness between the model's own
turns, so the turn-signal side can report idle while the hook-driven
attention axis (`attention::SessionStatus::attention`) still latches
`Attention::Approval`. Issue #468 observed exactly this in a live dashboard:
mail arrived while a receiving pane had a permission prompt open, and the
advisory was never typed -- not while the prompt was open, and not after it
closed either, because nothing in `mail_sweep` ever consulted the attention
axis at all.

Two failure modes follow from the same gap. Typing while the dialog is open
lands as raw keystrokes on the dialog itself (not a visible line an operator
reads), and can silently answer or garble the prompt. Not retrying once the
dialog closes means the mailbox sits unread indefinitely, since nothing else
in this codebase re-announces it (the operator in #468 only learned about
the mail because an unrelated background-task turn happened to fire the
`UserPromptSubmit` hook's own fallback advisory).

## Decision

Add one more gate, `mail_blocked_by_attention`, consulted by both
`sweep_one_pane` and `advise_one_pane` immediately before they would
otherwise inject: it loads the pane's `attention::SessionStatus`
(`attention::load`, the same read `report_stalled_compaction` already pays
once per relevant tick) and asks `attention::project` whether the session is
`Blocked` on a named `Attention` (Approval or otherwise -- `Blocked
(Attention::None)`, an unnamed `Lifecycle::Waiting`, is deliberately not
treated as a block, since nothing in this codebase latches that combination
from a live hook and it risks withholding ordinary mail from a session
merely waiting on its next prompt).

Retry is free: neither function consumes or dedup-marks a message while
blocked, so the very next sweep tick after `Pane::injectable()` and the
attention axis both clear tries again -- no new persisted "pending mail"
state was needed for that half of the fix, because #456/#457 already taught
Claude's `PreToolUse`/`PostToolUse`/`PermissionDenied` hooks to clear a
resolved `Approval` latch, and `attention::load` always reads the live file
rather than a dashboard-cached copy.

What *is* new is `Pane::mail_block_log: Option<(&'static str, String)>` (and
the matching `Injector::mail_block_log`/`set_mail_block_log` seam, so
`sweep_one_pane`/`advise_one_pane` stay testable without a real pty): the
mail id a skip was already logged for, so a dialog that stays open for many
~1s ticks produces one decision-log skip row, not one per tick, and the
eventual delivery logs a paired `mail-attention-delivered` row naming the
SAME mail id -- issue #468's own acceptance criterion ("a decision-log row
records the skip reason and the later delivery for the same mail id"). The
field is deliberately not persisted across a roster save/restore or a
handover swap (cleared there, alongside `report_reminder_sent`): it is a
diagnostic pairing, not the thing that actually matters (the unread mail
itself, still on disk).

## What changed

- `mail_blocked_by_attention` / `mail_block_reason` / `log_mail_attention_event`
  (`src/commands/ctx/dash/mod.rs`): pure gate, pure reason mapping, and the
  shared decision-log row shape (`action` = `mail-attention-skip` /
  `mail-attention-delivered`, `detail` carries `mail {id}: {reason}`).
- `sweep_one_pane`/`advise_one_pane`: gated on the above between their
  existing dedup checks and their injection; `sweep_one_pane` gained a
  `session_id` parameter (matching `advise_one_pane`'s own) purely for the
  decision-log `session` field.
- `Injector` trait: two new default (no-op) methods for the dedup pairing;
  `Pane` backs them with the new `mail_block_log` field.

## What is verified

Three inline tests against `advise_one_pane` (the orchestrator advisory --
the exact scenario #468 reproduced), cross-platform (no pty, no
`#[cfg(unix)]`):

- `advise_one_pane_never_types_while_approval_is_open`
- `advise_one_pane_retries_once_approval_clears_and_delivers_exactly_once`
- `attention_blocked_mail_logs_a_skip_and_a_matching_delivery_for_the_same_mail_id`

`sweep_one_pane` shares the identical gate and was re-verified against its
own existing suite (`a_sweep_delivers_exactly_one_message_per_pane_per_tick`
and friends), all still green.

## What is deferred

`wrap.rs`'s own standalone (non-dashboard) mail advisory (`MailWatch`,
`mail_inject_ready`) has the identical gap -- it gates purely on
`may_inject`'s own turn-signal question and never consults the attention
axis either. It was left out of this change: `wrap.rs` is a much larger,
`#[cfg(unix)]`-PTY-heavy module this Windows box cannot compile or run
directly (see CLAUDE.md's "Unix wrap tests need a Linux run"), the reported
bug and the focused gate filters for this task (`dash:: mail:: attention::
announce::`) both point at the dashboard path, and #468's own acceptance
tests are phrased against the dashboard advisory. A follow-up issue should
apply the same `attention::load`-based gate to `wrap::MailWatch`, verified on
Linux/Docker per the existing convention.
