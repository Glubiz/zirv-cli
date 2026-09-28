//! Supervises one headless run, restarting it on rot with a distilled
//! handoff. Mail (`super::mail`) is delivered into the composed system
//! prompt exactly once, at the very first launch computed in `run_with`: a
//! rot/timeout restart or a usage-limit park reuses that same launch's
//! `prompt_args` (the argv already carrying the composed text, whichever
//! mechanism delivered it), it does not recompute the composed prompt or
//! re-list mail. A message that arrives after the run has started is
//! therefore not retroactively injected into it -- the next `zirv ctx exec`
//! invocation (or a `zirv ctx loop` cycle, which re-lists mail every cycle
//! by design) picks it up instead.
//!
//! N4's `zirv ctx nudge` is the one deliberate exception: a nudge relaunch
//! recomposes the prompt and re-lists mail (scoped to the session that was
//! nudged) precisely because that recompute is the whole point -- see the
//! `nudged` branch in the main loop below.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{NormalizedEvent, SessionId, SessionRef, TranscriptUsage, input_hash};
use super::pace;
use super::rot::Verdict;
use super::signal::{self, TurnSignal};
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick};
#[cfg(test)]
use super::testenv;
use super::{CtxResult, adapters, agent, handoff, jev, jev_relay, log, objective, score};
use super::{
    announce, attention, config, dash, health, mail, obfuscate_store, prompt, provider, proxy,
    runtime, screen, sessions, stall, state,
};
use crate::commands::workflow::classify::Complexity;

mod accounting;
mod argv;
mod command;
mod compact;
mod effort;
mod entry;
mod restart;
mod supervision;

pub use self::accounting::ExecutionReport;
#[cfg(test)]
pub use self::accounting::ExecutionSegment;
use self::accounting::{
    evaluate_worker_budget, harvest_spend, objective_layer_for_restart, record_execution_segment,
};
pub(crate) use self::argv::pins_an_existing_conversation;
pub use self::argv::{extra_launch_flags, extract_prompt};
use self::argv::{locate_prompt, resume_pin};
use self::command::{build_command, headless_argv_len, headless_prompt_via_stdin};
pub(crate) use self::command::{headless_resume_launch, prompt_delivery_via_stdin};
use self::compact::protect_compaction_continuation;
pub(crate) use self::compact::{
    CompactBudget, action_for_verdict, compact_in_place, should_attempt_compact,
};
pub use self::compact::{SignalAction, action_for_signal};
use self::effort::apply_headless_cost_levers;
pub(crate) use self::effort::{
    LAUNCH_EFFORT_DEFAULT_FLOOR, launch_effort_action, launch_effort_question,
};
#[cfg(test)]
pub(crate) use self::entry::run_with_clock;
pub use self::entry::{ExecArgs, run, run_with, run_with_report};
pub(crate) use self::restart::EXIT_CODES;
use self::restart::capacity_backoff_secs;
pub use self::restart::{
    EXIT_ACCOUNT_EXHAUSTED, EXIT_BUDGET_EXHAUSTED, EXIT_CAPACITY_EXHAUSTED, EXIT_CONTRACT_FAILED,
    EXIT_ROT_EXHAUSTED, EXIT_STALLED, EXIT_TIMEOUT, EXIT_WRITER_BUSY, describe_exit, nudges_after,
};
use self::supervision::supervise_run;

#[allow(clippy::too_many_arguments)]
fn run_with_clock_inner<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
    stable_short: Option<&str>,
    // Issue #358 review finding #7: `true` for the very first launch of a
    // WHOLE delegation, `false` for a recursive re-entry this same function
    // makes on a provider-switch harness-handover restart (below). Without
    // this, that recursive call's own `initial_launch` local (T9) would
    // re-initialise to `true` on its own first loop iteration -- a brand
    // fresh `run_with_clock_inner` call frame has no memory of the frame
    // that tail-called it -- so a mid-delegation provider switch would skip
    // the pacing wait exactly like a genuine first launch, even into a
    // provider that is `WaitUntil`.
    initial_launch_allowed: bool,
    report: &mut ExecutionReport,
    // Issue #690 (remaining scope): the launch pre-flight's presence oracle,
    // threaded through rather than read from the ambient `PATH` -- see
    // `run_with_clock_and_presence`'s own doc comment. Passed on unchanged to
    // the recursive re-entry below, so a provider-switch harness handover
    // pre-flights against the same stated machine this call did.
    present: &dyn Fn(&str, &str) -> adapters::Liveness,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Gated only by `cfg.chrome.events` (which already folds in `--quiet` on
    // `zirv ctx agent`, `ZIRV_CTX_QUIET` and `[chrome] events`), independent
    // of whatever terminal (if any) is attached: a headless supervised run
    // still wants these lines on its stderr.
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    // Issue #690 (remaining scope): whether this run's own spawn is the
    // adapter's own program (zirv builds the launch) or the operator's
    // explicit `-- <command>`. Resolved here rather than at its former
    // position ~90 lines below, because selection and the launch pre-flight
    // immediately after are the first things that need it, and the
    // pre-flight must run before pacing, usage polling and the macOS
    // Keychain-reading path they drag in (`pace::wait_for_window`, ~700
    // lines below). It reads `args` alone, so hoisting it can change nothing
    // else; `prefix`, which also needs the resolved adapter, stays where it
    // was.
    let adapter_builds_launch = args
        .command
        .first()
        .is_none_or(|first| first.starts_with('-'));
    // `select_with_presence` rather than `select`, stating the same
    // `adapter_builds_launch` the pre-flight below is gated on and handing
    // it the same injected oracle: one stated machine governs both halves of
    // this launch. `select`'s own derivation (`command.is_empty()`) is
    // `wrap`'s reading of a wrapped argv -- there the command IS the program
    // to spawn -- and it is too narrow here: a flags-only `-- --model x` is
    // adapter-built for `exec`, which appends those flags to
    // `adapter.program()`. Deriving it there made `zirv ctx exec -- --model
    // x` keep a default harness this machine does not have and then refuse
    // it at the pre-flight, on a machine with another one installed.
    let adapter = adapters::select_with_presence(
        agent_name,
        &args.command,
        &cfg,
        adapter_builds_launch,
        present,
    )?;
    // Issue #690 (remaining scope): the launch pre-flight -- a harness that
    // is confidently not on this machine fails here, immediately, instead of
    // after a Keychain advisory and a blind-mode safety delay for a harness
    // the operator does not have. Gated on `adapter_builds_launch` because
    // that is exactly the condition under which the program about to be
    // spawned IS `adapter.program()`: an explicit `-- <command>` is the
    // operator's own argv, which this check has no business refusing (see
    // `adapters::refuse_if_program_absent_with_presence`'s own doc comment).
    // Fail-open and never substituting, both by construction there.
    if adapter_builds_launch {
        adapters::refuse_if_program_absent_with_presence(adapter.as_ref(), &cfg, present)?;
    }
    let execution_started = Instant::now();
    let execution_model = adapters::last_model_flag(&args.command).map(str::to_string);
    // Issue #155 review finding C2: refused here, before anything is
    // spawned, rather than left to silently never fire -- see
    // `AgentAdapter::counts_tool_calls`'s own doc comment for why this
    // adapter cannot enforce the flag at all.
    if args.max_tool_calls.is_some() && !adapter.counts_tool_calls() {
        return Err(format!(
            "--max-tool-calls is not supported with the '{}' adapter: it has no verified way \
             to count tool calls in its transcript, so the ceiling would never be enforced",
            adapter.name()
        )
        .into());
    }
    // Resolved once, since `adapter` never changes across a nudge/rot/park
    // restart within one `run_with` call: the operator's own choice
    // (`handoff.model`) if set, else the resolved adapter's own default
    // (claude: "haiku"; codex: none, which `CodexAdapter::distiller_cmd`
    // reads as "omit --model").
    let distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    let state = StateDir::resolve(env)?;
    // Still needed standalone: the mail layer below needs a slug, and issue
    // #44's `compile::compile` (which now owns reading the memory bank; see
    // its own call below) computes this same slug internally but callers
    // still need their own copy for mail listing.
    let mail_slug = super::state::repo_slug(repo);
    // Issue #249: this run's own supervising session, if any -- resolved
    // once, from `env` alone, and reused at every mail-rendering call below
    // (the launch-time delivery and every relaunch arm), never re-derived
    // from anything a message itself carries.
    let parent_short = agent::parent_identity(env);

    // Issue #285: `--objective` sets (or replaces) this repository's durable
    // objective once, before the first launch below, so it is picked up by
    // the very first `compile::compile` call. A shorthand for `zirv ctx
    // objective set`; its own soft budget defaults from `[pace] run_budget_
    // tokens`, the same fallback `objective::run_set` applies. Never re-run
    // on a nudge/rot/park/harness-handover restart within this same call --
    // those reload the SAME durable record (see `objective_layer_for_
    // restart` below) rather than resetting it.
    if let Some(text) = &args.objective {
        let key = super::state::repo_slug(repo);
        let record = objective::Objective {
            schema_version: objective::SCHEMA_VERSION,
            objective: text.clone(),
            budget_tokens: cfg.pace.run_budget_tokens,
            deadline_secs: None,
            spent_tokens: 0,
            started_at: now_fn(),
            status: objective::Status::Active,
            pending_note: None,
            evidence: Vec::new(),
        };
        objective::store(&state, &key, &record)?;
    }

    // A wrapped command that matches no adapter (no explicit `--agent`,
    // detection came up empty) is not actually the agent whose flags we would
    // be injecting; see the matching gate in wrap.rs.
    let skip_injection = args.simple
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    // Issue #44: gathers memory, the canonical `.zirv/context/` layer, and
    // attaches the policy report -- see `compile::compile`'s own doc
    // comment. A Worker session never hears about the derived harness
    // roster either way; see `prompt::PromptSource::Harnesses`.
    let mut compiled = super::compile::compile(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        skip_injection,
        &cfg,
        adapter.as_ref(),
        super::prompt::PromptRole::Worker,
        &state,
        now_secs(),
        super::adapters::LaunchMode::Headless,
        true,
    );
    // Known before argv is touched, because it decides how argv is read: the
    // token holding this exact text is the prompt, whatever it looks like.
    let prompt = args
        .prompt
        .clone()
        .or_else(|| extract_prompt(&args.command));
    if let Some(task) = prompt.as_deref() {
        super::compile::select_skill_descriptions_for_task(
            &mut compiled,
            &cfg,
            &state,
            repo,
            crate::utils::home_dir().ok().as_deref(),
            task,
        );
    }
    let composed = compiled.composed;
    // An argv that names no program -- empty, or starting with a flag -- is
    // not a command to pass through: the adapter builds the launch and these
    // are extra flags for it. That is how an agent step arrives, holding its
    // prompt as data with no argv to encode it into. (`adapter_builds_launch`
    // itself is now resolved just above `adapters::select_with_presence`,
    // which is handed it, and the issue #690 launch pre-flight right after.)
    let prefix = if adapter_builds_launch {
        0
    } else {
        adapter.launch_prefix_len()
    };
    // The prompt is data, not argv to be interpreted. Protecting its index
    // keeps a prompt that happens to read like the adapter's own
    // system-prompt flag from being stripped out of the launch and promoted
    // into the composed prompt as an operator instruction.
    let prompt_value_at = locate_prompt(&args.command, prefix, prompt.as_deref())
        .and_then(|(index, value)| value.map(|_| index + 1));

    // Issue #778: the operator's own trailing args may already name an
    // existing conversation to resume (`-- --resume <id>`), for zirv to
    // track under that SAME id -- transcript derivation, the registry short
    // id and every decision-log entry below -- rather than a fresh, unrelated
    // one that has nothing to do with the conversation actually being
    // resumed. `resume_pin` reads the same `RESUME_FLAGS_WITH_VALUE`/
    // `RESUME_FLAGS_BARE` list `pins_an_existing_conversation`/`extra_launch_
    // flags` already use; `resume_pin_tokens` (its other half) is consulted
    // further down, only for the very first launch's own `extra`, once
    // `user_extra` below has already stripped them the same way it would for
    // a restart.
    let (resume_pin_tokens, resumed_session_id) = resume_pin(&args.command, adapter.name());
    // Determined before mail is listed (N3: delivery is scoped to this
    // session's own short id, so the id has to exist first) and before
    // `prompt_args` (M7 needs a session id to name the private prompt file
    // after) rather than after, as this used to be. `args.session_id` --
    // zirv's own flag -- still wins outright over a resumed id: an operator
    // who names both gets what they explicitly pinned.
    let session_raw = args
        .session_id
        .clone()
        .or(resumed_session_id)
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);

    // Mail is delivered once, here, at the first launch: every restart below
    // reuses this same `composed` value (see the module doc), so a message
    // that arrives mid-run is not retroactively injected into an
    // already-running session. `run_loop`, by contrast, starts a fresh
    // session every cycle and re-lists mail on each one.
    //
    // `mut`: drained by the loop below, once, right after the first
    // successful spawn -- not here. Consuming this early (Item 3's fix) used
    // to mark the mail read before any child had actually started: a launch
    // that fails to spawn at all, or a long pacing park ahead of it, moved
    // it to `read/` with no session ever having seen it.
    //
    // N3: scoped to this run's own short id, so a message addressed to a
    // different session (`send --to-session`) never leaks into this launch's
    // prompt just because the two share a repo and an agent name.
    //
    // C7: this is the *registry* short -- the address `SessionGuard` files
    // this run under below and keeps stable for its whole lifetime (see
    // `SessionGuard::refresh_session`). Every later listing in this function
    // reuses this exact value rather than recomputing `short_id(session)`,
    // which rotates on every restart and stranded any mail addressed to the
    // session a sender had actually resolved.
    let registry_short = stable_short
        .map(str::to_string)
        .unwrap_or_else(|| super::sessions::short_id(session.as_str()));
    // An adapter with no system-prompt injection mechanism never reaches
    // `injection_args_for_session`'s output at all -- folding mail into
    // `composed` for one only would silently destroy it, so for such an
    // adapter it is instead appended straight onto the task prompt text
    // below (`task_prompt_with_mail_fallback`), the one channel such an
    // adapter does have. A capable adapter (claude) is unaffected either
    // way: this still folds mail into `composed` exactly as before.
    let system_prompt_supported = adapter.system_prompt_supported(&args.command);
    // But the task-prompt fallback only exists when zirv itself builds the
    // launch (`adapter_builds_launch`): when the caller passed an explicit
    // command (`-- codex exec "task" ...`), that argv is fixed by the caller
    // and zirv has no task-prompt text of its own to append a fallback to.
    // Rather than list mail this *initial launch* can never actually deliver
    // -- and then either destroy it by consuming an undelivered batch, or
    // strand it marked-unread-forever after a later restart silently did
    // deliver it -- it is left untouched in the mailbox entirely: still
    // visible to `zirv ctx inbox`, and to any other session (or this same
    // run's own later restart) that can actually deliver it.
    //
    // Final wave item 2: `mail_deliverable` restricts *only* this initial
    // launch's own listing (below), not any later restart. Every relaunch
    // arm -- nudge, limit-park, rot/timeout -- rebuilds through `build_
    // headless`, which is unconditionally zirv's own launch regardless of
    // what the original invocation's argv looked like, so the task-prompt-
    // text channel exists on every one of them even when it did not exist
    // at launch. The nudge arm accordingly lists mail fresh without this
    // flag; the park and rot-restart arms don't re-list at all, but reuse
    // whatever `mail_messages` currently holds -- the launch-time listing,
    // or a nudge's own fresher one if this run was nudged first (Medium 4
    // keeps `mail_messages` in lockstep with `mail_entries` wherever either
    // is reassigned).
    let mail_deliverable = adapter_builds_launch || system_prompt_supported;
    // Item 14: `composed.is_some()` only gates listing for an adapter whose
    // *only* delivery channel is `composed` (claude): under `--simple`
    // (`skip_injection`, so `composed` is always `None` regardless of
    // adapter), that used to also withhold mail from an injection-less
    // adapter (codex) whose real channel -- the task-prompt text,
    // `task_prompt_with_mail_fallback` further down -- exists entirely
    // independently of `composed` and does not care whether it is `--simple`
    // or not. `!system_prompt_supported` is the other way in.
    let mut mail_entries: Vec<(PathBuf, super::mail::Message)> =
        if cfg.mail.enabled && mail_deliverable && (composed.is_some() || adapter_builds_launch) {
            super::mail::list(
                &state,
                &mail_slug,
                Some(adapter.name()),
                super::sessions::delivery_filter(None, &registry_short),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
    // Low 8: `mail_deliverable == false` means this launch never lists mail
    // above at all (there is nowhere for it to go), so an operator watching
    // the `zirv ▸` channel saw nothing and had no way to tell "no mail was
    // pending" from "mail was pending but silently withheld" -- exactly the
    // visibility `dash/mod.rs`'s own worker-pane spawn already gives via
    // `push_error` for its narrower shim-unsafe case. A read-only listing,
    // never consumed here (this launch cannot deliver it, so it must stay
    // unread), just to say whether there is anything to announce.
    if cfg.mail.enabled && !mail_deliverable {
        let withheld = super::mail::list(
            &state,
            &mail_slug,
            Some(adapter.name()),
            super::sessions::delivery_filter(None, &registry_short),
        )
        .unwrap_or_default();
        if !withheld.is_empty() {
            announcer.emit(&super::announce::Event::MailWithheld {
                count: withheld.len(),
            });
        }
    }
    // Medium 4: `mut` -- kept in lockstep with `mail_entries` wherever that
    // is reassigned (the nudge arm, below), so a later park/rot-restart's
    // own `task_prompt_with_mail_fallback` call (which intentionally reuses
    // whatever this holds rather than re-listing) sees the most recent
    // listing, not permanently the launch-time one.
    let mut mail_messages: Vec<super::mail::Message> = mail_entries
        .iter()
        .map(|(path, msg)| {
            super::mail::message_with_delivery_envelope(
                &cfg,
                &state,
                path,
                msg,
                parent_short.as_deref(),
                &cfg.screen.thresholds(),
            )
        })
        .collect();
    if !mail_messages.is_empty() {
        announcer.emit(&super::announce::Event::MailDelivered {
            count: mail_messages.len(),
        });
    }
    let composed = if system_prompt_supported {
        super::prompt::with_mail_layer(
            composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
            parent_short.as_deref(),
        )
    } else {
        composed
    };

    // The first spawn's own argv may already carry the adapter's system-prompt
    // flag (e.g. `-- claude --append-system-prompt "..."`); merge it in rather
    // than letting `prompt_args` silently override it below.
    // `mut`: a nudge relaunch (N4) recomposes fresh, mail included, and
    // replaces this binding so any restart or park after it keeps using the
    // nudge-enriched prompt rather than silently reverting to the launch-time
    // one.
    // PLAUSIBLE-1 (confirmed): captured here, at launch, because this is the
    // only point the operator's own prompt flag is still present in argv.
    // `merge_command_line_prompt` strips it, and every relaunch below holds
    // the *cleaned* argv -- so re-running the merge on a relaunch found
    // nothing and silently dropped the operator's instruction from the
    // recomposed prompt. `relayer_recomposed` re-applies this instead.
    let operator_prompt_text: Option<String> = if composed.is_some() {
        super::prompt::extract_user_prompt_flag(adapter.as_ref(), &args.command, prompt_value_at)
            .ok()
            .and_then(|(_, text)| text)
    } else {
        None
    };
    let (launch_command, mut composed) = super::prompt::merge_command_line_prompt(
        adapter.as_ref(),
        &args.command,
        composed,
        prompt_value_at,
        super::prompt::PromptRole::Worker,
        &cfg.prompt,
    );
    composed = super::obfuscate_store::protect_composed(
        &state,
        repo,
        &cfg,
        composed,
        "exec_system_prompt",
    )?;

    // The user's own flags from the original `--` command (anything beyond
    // the prompt and the session-pinning flags, all of which every restart
    // regenerates fresh): see `extra_launch_flags`. M8: a restart used to
    // rebuild the command from scratch with only zirv's own added flags,
    // silently dropping these.
    let user_extra = extra_launch_flags(&launch_command, prefix, prompt.as_deref(), adapter.name());
    // Bug B (harness/model parity, 2026-08-22): the shipped-default
    // "sandboxed, no prompts" posture (`SandboxConfig`) plus any explicit
    // `[policy]` restriction, from the same seam every other real launch now
    // calls (`adapters::policy_launch_args`). Computed once here, ahead of
    // `user_extra` at every one of this function's four launch-building
    // sites (initial launch, nudge/park/rot-timeout relaunches), the same
    // discipline `user_extra` itself already follows -- see that binding's
    // own comment. `flags_pin_policy` (inside `policy_launch_args`) reads
    // `user_extra`, not the raw wrapped command, so an operator's own
    // `--sandbox`/`--ask-for-approval`/`--permission-mode`/
    // `--disallowedTools` anywhere in their own trailing flags still wins.
    //
    // Deliberately **not** gated on `skip_injection` (which also folds in
    // `args.simple`): `--simple` promises no *injected instruction text*,
    // and the sandbox posture is a safety flag layer, not instruction text
    // (mirrors `wrap.rs`'s identical `policy_skip` reasoning, and `chat.rs`'s
    // own `--simple` test). It is still gated on the one reason `skip_
    // injection` exists that *does* apply here: a wrapped command that does
    // not actually match this adapter must never receive this adapter's
    // flags -- the same leakage risk `skip_injection` exists to prevent for
    // `prompt_args`. `adapter_builds_launch` is exempt from that check
    // entirely: when zirv builds the launch itself (from `--prompt`, no
    // explicit `-- <command>`), there is no wrapped command to mismatch --
    // it is unconditionally this adapter's own launch.
    let policy_skip = !adapter_builds_launch
        && !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let mut policy_extra = if policy_skip {
        Vec::new()
    } else {
        adapters::policy_launch_args(
            &cfg,
            adapter.as_ref(),
            &user_extra,
            adapters::LaunchMode::Headless,
            super::prompt::PromptRole::Worker,
        )
    };
    // Visible, not silent: the shipped-default posture (or the operator's
    // own opt-out/override) is announced once, here, at session start --
    // not re-announced on a nudge/rot/park relaunch, since `policy_extra`
    // itself is computed once above and simply reused by every relaunch arm.
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if policy_extra.is_empty() {
            "not applied (operator flags, --simple/command mismatch is irrelevant here, or \
             [sandbox] enabled = false)"
                .to_string()
        } else {
            super::announce::posture_detail(&policy_extra)
        },
    });
    if !policy_skip {
        let mcp_args = super::mcp::launch::arguments(
            adapter.name(),
            repo,
            &state,
            &registry_short,
            &user_extra,
        );
        super::mcp::launch::append(&mut policy_extra, mcp_args);
    }
    // Issue #420: heal any self-healable (`Outdated`) hook entry, then warn
    // at most once per 24h if something still drifted. Best-effort: no home
    // directory is not a reason to fail the launch.
    if let Ok(home) = crate::utils::home_dir() {
        let _ = super::hook_integrity::heal_outdated(&state, &home);
        if let Some(summary) = super::hook_integrity::drift_warning_if_due(&state, &home) {
            announcer.emit(&super::announce::Event::HookIntegrity { summary });
        }
    }

    // The probe has to hit the binary that will actually be spawned. When the
    // argv names no program the adapter builds the launch, so there is nothing
    // in `launch_command` to probe -- it is flags, and `--model --help` is not
    // a capability check.
    let probe_target: &[String] = if adapter_builds_launch {
        &[]
    } else {
        &launch_command
    };
    // `mut`: recomputed by a nudge relaunch alongside `composed` above.
    let mut prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        probe_target,
        composed.as_ref(),
        &state,
        session.as_str(),
    )?;
    super::prompt::log_injection(
        &state,
        "exec",
        session.as_str(),
        composed.as_ref(),
        system_prompt_supported,
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        system_prompt_supported,
    ));

    let derive_transcript = |session: &SessionId| {
        adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    };

    // `--transcript` describes the caller's own first child only. Every restart
    // is a new session launched by the adapter, so its transcript path has to be
    // derived again or the watcher would keep polling the dead child's file.
    let mut transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| derive_transcript(&session));
    // Review round 2 (S1): gates the tick's transcript self-heal
    // (`self_heal_transcript`). False only for the caller's own
    // `--transcript`, and only until the first restart re-derives -- every
    // path zirv derived itself may be re-resolved, an operator's may not.
    let mut transcript_derived = args.transcript.is_none();

    // Surfaced once, upfront, rather than only when a restart is already
    // needed: an operator who never rots would otherwise never learn that
    // rotting is a dead end for this invocation until it actually happens.
    if prompt.is_none() {
        writeln!(
            w,
            "zirv ctx exec: no prompt could be found in the command; restarts and usage-limit \
             parking will be unavailable for this run. Pass --prompt to enable them."
        )?;
    }
    let max_restarts = args.max_restarts.unwrap_or(cfg.supervise.max_restarts);
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);
    // Issue #155, Phase 5(d): the CEILING is fixed for the whole run, same as
    // `max_restarts`/`timeout` above. Issue #169.2: SPEND is now accumulated
    // across every restart this run mints, not just measured against
    // whichever child happens to be running -- see `prior_usage`/`prior_
    // tool_calls` below, harvested from each outgoing transcript at every
    // restart/nudge/park site before a fresh one is minted. Before this fix
    // a rot/timeout/nudge restart or a usage-limit park -- all of which mint
    // a fresh transcript -- silently reset the meter, so N restarts allowed
    // N times the configured ceiling.
    let worker_budget = agent::WorkerBudget {
        tokens: args.budget_tokens,
        tool_calls: args.max_tool_calls,
    };
    // Issue #169.2: the running total of every PRIOR child's own spend this
    // invocation has already superseded (a rot/timeout/nudge restart, or a
    // usage-limit park). Folded into every budget check alongside the
    // current child's own transcript (`evaluate_worker_budget`), so the
    // budget bounds the whole supervised run, not just its latest incarnation.
    let mut prior_usage = TranscriptUsage::default();
    let mut prior_tool_calls: u32 = 0;

    let socket_path = state.socket_for(session.as_str());
    let server = match signal::SignalServer::bind(&socket_path) {
        Ok(server) => Some(server),
        Err(e) => {
            // Turn signals only accelerate detection; polling is the floor.
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "no-socket",
                    detail: &e.to_string(),
                    observed_at: None,
                },
            );
            None
        }
    };
    // Rebuilt for every session, because the hook inside a child reports the
    // session id this exports. Pinning the first one makes every restart's
    // signals look like they belong to a session that is already dead.
    //
    // `AGENT_ENV` is exported unconditionally, unlike the turn-signal env
    // above (which needs a bound socket): it names the same fact
    // `ctx.toml`'s own `agent` config key would, so a nested `zirv ctx ...`
    // call inside this session's own children defaults to this session's own
    // harness rather than re-resolving from scratch, whether or not turn
    // signals are available.
    let turn_env_for = |session: &SessionId| {
        let mut turn_env: Vec<(String, String)> = server
            .as_ref()
            .map(|server| {
                adapter
                    .register_turn_signal(
                        &SessionRef {
                            id: session.clone(),
                            cwd: repo.to_path_buf(),
                        },
                        server.path(),
                    )
                    .env
            })
            .unwrap_or_default();
        turn_env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
        // Issue #800: the actual, resolved route this launch took, so a
        // later reconciliation (`zirv workflow spend`, `OutcomeRow::direct`)
        // never has to re-derive it from argv/config itself. `tier` is
        // derived through the same handover ladder `handover::resolve_model`
        // itself uses; `effort` is left unset here (`CLAUDE_CODE_EFFORT_
        // LEVEL` is set directly on the child `Command`, further down this
        // same function, not on this env vec).
        let route_tier = execution_model
            .as_deref()
            .and_then(|model| super::handover::tier_for_model(adapter.name(), model, &cfg));
        turn_env.extend(super::attribution::route_env(
            Some(adapter.name()),
            execution_model.as_deref(),
            route_tier,
            None,
        ));
        // Security review round 2 (Finding 3): the work-group binding travels
        // by lineage. `dash::fulfill_spawn_request` already pushed this exact
        // pair into a pane's own `turn_env`; the headless launch pushed
        // nothing, so a headless sub-orchestrator's children resolved
        // `group = None` (`agent::resolve_group_binding`'s env fallback found
        // nothing) -- no `admit_child`, no `child_limit`, no token ceiling,
        // and every such delegation rendered "ungrouped" in `zirv ctx
        // status`'s group tree. Read from this run's own env lookup, which
        // `agent::run_with` folds its resolved `--group` into (`group_env`,
        // the same shape `chat::quiet_env` established), so an inherited
        // binding and a freshly resolved one reach the child identically.
        if let Some(group) = env(super::agent::WORK_GROUP_ENV).filter(|id| !id.is_empty()) {
            turn_env.push((super::agent::WORK_GROUP_ENV.to_string(), group));
        }
        // Issue #249: this run's own supervising session, if any, exported
        // into the CHILD's real process environment -- not merely read by
        // this supervisor's own in-process mail-rendering (`parent_short`,
        // above). The child is what actually runs `zirv ctx send`/`zirv ctx
        // inbox` as its own report-back/steering channel, as a brand new OS
        // process that inherits nothing from this Rust closure, so it needs
        // its own copy of the same fact. `agent::run_with`'s `parent_
        // session_env` fold already resolved this to the delegating
        // session's own id (never a stray inherited value -- see that
        // fold's own doc comment), so a plain re-read here is exactly right.
        if let Some(parent) = env(super::agent::PARENT_SESSION_ENV) {
            turn_env.push((super::agent::PARENT_SESSION_ENV.to_string(), parent));
        }
        // Issue #318: the same lineage-by-env shape as `WORK_GROUP_ENV`/
        // `PARENT_SESSION_ENV` immediately above -- `agent::run_with` folds
        // its resolved `--result-schema`/`--result-kind` into this run's own
        // env lookup (`agent::result_schema_env`), so a headless child sees
        // the OUTPUT CONTRACT it must report against, and its own `zirv ctx
        // send --to-session` self-report (should it use one) validates
        // against the identical contract the headless retry path enforces.
        if let Some(schema) = env(super::agent::RESULT_SCHEMA_ENV).filter(|s| !s.is_empty()) {
            turn_env.push((super::agent::RESULT_SCHEMA_ENV.to_string(), schema));
            turn_env.push((
                super::agent::RESULT_WORKDIR_ENV.to_string(),
                repo.display().to_string(),
            ));
        }
        turn_env
    };

    // F3: the one place a launch's session identity is applied, so the scrub
    // can never be forgotten on one of the four relaunch paths below. The
    // scrub is unconditional and comes first: `turn_env_for` yields nothing
    // when the socket bind failed, and without this the child inherited the
    // *outer* session's `ZIRV_CTX_SESSION`/`ZIRV_CTX_SOCKET` from this
    // process's own environment and reported its turns into somebody else's
    // supervisor. A worker legitimately runs inside a session (that is what
    // `zirv ctx agent` is), but it must still speak with its own identity or
    // none at all.
    // Issue jev-relay: the relay is (re)hosted from inside this same closure
    // rather than at each of its own call sites, since this is already "the
    // one place a launch's session identity is applied" for every relaunch
    // path -- see this closure's own doc comment above. Rebinding only when
    // `session` actually changed since the last call (`jev_relay_session`)
    // keeps a same-session re-application (the in-place compaction arms
    // below) from tearing down and rebuilding a perfectly live relay for no
    // reason.
    let mut jev_relay_handle: Option<jev_relay::Handle> = None;
    let mut jev_relay_session: Option<String> = None;
    let mut apply_session_env = |command: &mut Command, session: &SessionId| {
        if jev_relay_session.as_deref() != Some(session.as_str()) {
            jev_relay_handle = jev_relay::start(
                &cfg.proxy.typesafe,
                jev::any_gate_enabled(&cfg.jev),
                &state,
                session.as_str(),
            );
            jev_relay_session = Some(session.as_str().to_string());
        }
        super::sessions::scrub_supervision_env_cmd(command);
        for (key, value) in turn_env_for(session) {
            command.env(key, value);
        }
        // Issue #236: this module supervises only `LaunchMode::Headless`
        // runs -- nobody is present to answer a permission prompt -- so
        // every child it spawns gets that mode's marker, read by
        // `engine::refusal_for` to refuse the interactive `brainstorm`
        // skill. Derived from the mode itself (2026-09-06) rather than
        // hardcoded here, so this seam and a pane's own cannot drift.
        // Scrubbed by `scrub_supervision_env_cmd` above first, so a nested
        // launch never inherits a stale copy before this sets its own.
        if let Some((key, value)) = adapters::headless_marker_env(adapters::LaunchMode::Headless) {
            command.env(key, value);
        }
    };

    // FIX B: on a Windows npm `.cmd` shim launch, `cmd.exe /c <shim>` reparses
    // the whole downstream argv, so a headless prompt on argv -- operator task
    // text, plus any mail folded into a nudge/restart relaunch below -- would
    // be reinterpreted by cmd.exe. Deliver it on the child's stdin instead (the
    // same mechanism `handoff::run_model`'s distiller uses), and only on that
    // launch shape: off Windows, and for a directly executable `.exe`, the
    // prompt stays on argv exactly as before, so every `sh`-based fake-agent
    // test is byte-identical. Returns the built command and the stdin payload
    // (`Some` only when the prompt was kept off argv).
    //
    // Final wave item 1: `adapter.launches_through_cmd_shim()` only
    // recognises the `cmd.exe /c <shim>` form -- a `.ps1`-resolved
    // `agent_bin` would report "safe" here while `headless_cmd`'s own argv
    // (built below, on the `false` branch) still reached a `powershell
    // -File` launch with the prompt on the reparsed argv, the same M1 gap
    // dash/mod.rs's `task_prompt_fallback_is_safe` closed for the pty path.
    // The probe below builds exactly the launcher prefix this run's real
    // headless spawn will use (`headless_cmd("", ...)` -- no prompt token
    // yet, since deciding whether one is safe to put there is the point)
    // and asks `launch_reparses_through_shim`, which covers both forms.
    //
    // Final wave item 2: no longer ANDed with `adapter_builds_launch`.
    // `prompt_via_stdin` is consulted only inside `build_headless` below,
    // and `build_headless` is what *every* relaunch (nudge, park, rot/
    // timeout) uses regardless of what the *initial* launch looked like --
    // wave 5's item 2 made that explicit for mail deliverability, and the
    // same fact applies here: an explicit `-- <command>` at the initial
    // launch (`adapter_builds_launch == false`) does not stop a later
    // relaunch from rebuilding through `build_headless` on a shim-resolved
    // agent. With the old conjunct, `prompt_via_stdin` was pinned `false`
    // for that whole run, so a relaunch's multi-line composed/mail prompt
    // text landed on argv instead of stdin and `guard_cmd_shim_reparse`
    // aborted the run outright the moment one arrived -- pre-existing (it
    // affects claude too, not just codex), just widened by wave 5's own fix
    // making relaunches reachable in more shapes than before.
    let prompt_via_stdin = prompt_delivery_via_stdin(adapter.as_ref(), &session);
    let relaunch_system_prompt_supported = adapter.system_prompt_supported(&[]);
    // Issue #220: `headless_prompt_via_stdin` also routes an oversized
    // launch to stdin regardless of `prompt_via_stdin` -- see its own doc
    // comment. Measured per call, not hoisted out here as a single flag,
    // because `build_headless` is the one chokepoint every relaunch --
    // nudge, park, rot restart -- reuses with its own, possibly
    // differently-sized, `prompt_text`/`extra` (see the call sites' own
    // comments). Correctness follow-up (post-merge review): the probe
    // measures the FULL `adapter.headless_cmd(prompt_text, session, extra)`
    // argv -- not just `prompt_text.len()` -- because the #213 system-prompt
    // layer folded into `extra` rides the same command line and can itself
    // occupy close to the whole budget, so a prompt safely under budget on
    // its own could still leave the total argv over it. Built once and
    // reused as the argv-delivery fallback below, rather than built twice.
    let build_headless = |prompt_text: &str,
                          session: &SessionId,
                          extra: &[String]|
     -> CtxResult<(Command, Option<String>)> {
        let prompt_text = super::obfuscate_store::protect_text(
            &state,
            repo,
            &cfg,
            prompt_text,
            "exec_task_prompt",
        )?
        .0;
        let probe = adapter.headless_cmd(&prompt_text, session, extra);
        let argv_total_len = headless_argv_len(&probe);
        if headless_prompt_via_stdin(prompt_via_stdin, argv_total_len)
            && let Some(mut command) = adapter.headless_cmd_stdin(session, extra)
        {
            apply_headless_cost_levers(
                &mut command,
                &cfg,
                adapter.name(),
                Some(&prompt_text),
                &state,
                session,
            );
            return Ok((command, Some(prompt_text)));
        }
        let mut probe = probe;
        apply_headless_cost_levers(
            &mut probe,
            &cfg,
            adapter.name(),
            Some(&prompt_text),
            &state,
            session,
        );
        Ok((probe, None))
    };

    // With no argv to pass through, the first launch is built exactly the way
    // every relaunch builds one. That symmetry is the point: a caller holding
    // the prompt as data never encodes it into argv for this function to
    // decode again, so it can never be misread as a flag.
    let (mut command, mut stdin_prompt) = if adapter_builds_launch {
        let prompt_text = prompt.as_deref().ok_or(
            "no command to supervise; pass the agent command after --, \
             or --prompt to have zirv build the launch itself",
        )?;
        let prompt_text = super::prompt::task_prompt_with_composed_fallback(
            prompt_text,
            system_prompt_supported,
            composed.as_ref(),
        );
        let mail_in_composed = composed
            .as_ref()
            .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
        let prompt_text = super::prompt::task_prompt_with_mail_fallback(
            &prompt_text,
            (system_prompt_supported && composed.is_some()) || mail_in_composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
            parent_short.as_deref(),
        );
        // Issue #778: `resume_pin_tokens` re-adds, for this very first launch
        // only, exactly the resume-pinning flag `user_extra` above already
        // stripped out (`extra_launch_flags`'s own resume-flag handling,
        // unchanged, still governs every relaunch below via the same
        // `user_extra` binding) -- so an operator's own `-- --resume <id>`/
        // `--continue` reaches the adapter's argv here, and `ClaudeAdapter::
        // headless_cmd` sees it and skips minting its own conflicting
        // `--session-id`.
        let extra: Vec<String> = policy_extra
            .iter()
            .cloned()
            .chain(user_extra.iter().cloned())
            .chain(resume_pin_tokens.iter().cloned())
            .chain(prompt_args.iter().cloned())
            .collect();
        let (mut command, stdin_prompt) = build_headless(&prompt_text, &session, &extra)?;
        command.current_dir(repo);
        (command, stdin_prompt)
    } else {
        // An explicit `-- <command>` is the operator's own fixed argv: there
        // is no `user_extra` slot to prepend `policy_extra` ahead of, so
        // both zirv-owned additions are appended the same way `prompt_args`
        // already was here, before this fix -- see `policy_extra`'s own
        // comment for why `flags_pin_policy` still consulted the launch's
        // own trailing flags (folded into `user_extra` above) rather than
        // this branch's fixed command.
        let mut argv = launch_command.clone();
        super::mcp::launch::append(
            &mut argv,
            policy_extra
                .iter()
                .chain(prompt_args.iter())
                .cloned()
                .collect(),
        );
        let mut command = build_command(&argv, repo)?;
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            adapter.name(),
            prompt.as_deref(),
            &state,
            &session,
        );
        (command, None)
    };
    apply_session_env(&mut command, &session);
    let mut restarts = 0;
    // N4: consecutive `zirv ctx nudge`-driven restarts, capped by `cfg.
    // supervise.max_nudges` -- a separate budget from `restarts` above,
    // since a nudge is not rot and must never spend it. A relaunch (nudge or
    // otherwise) needs a known prompt to carry forward; without one a nudge
    // is claimed but ignored, the same as being over the cap.
    let mut nudge_restarts = 0u32;
    let can_restart = prompt.is_some();

    // Best-effort registration: covers a hand-typed `zirv ctx exec` as well
    // as `zirv ctx agent` and a script `agent:` step, both of which delegate
    // to this same function. Refreshed (not re-registered) whenever a
    // restart or a usage-limit park mints a fresh session id below, and
    // released explicitly in every arm that leaves this loop -- the same
    // explicit-arm discipline `RawGuard` follows, since this binary's
    // release profile is `panic = "abort"` and `Drop` is not guaranteed.
    // Issue #139: see `wrap.rs::run_with`'s identical comment -- pure and
    // deterministic from the same `cfg.safety` this launch's own settings
    // file was built from, so `status.rs` can later detect a widened policy
    // this session's own launch snapshot has not adopted yet.
    //
    // Issue #155, Phase 5(e): the former heavy-worker registration gate
    // (`sessions::count_heavy_workers`, refusing a launch outright at this
    // point) is gone -- a session registration is no longer a heavy event by
    // itself. The machine-wide budget now gates the actual heavy COMMAND, at
    // `script_runner::Command::invoke` (`permit::acquire`), so an idle
    // supervised session here holds nothing.
    let safety_policy_sha256 = super::safety::policy_fingerprint(&cfg.safety).ok();
    let mut session_guard = super::sessions::SessionGuard::register(
        &state,
        super::sessions::Record::new(
            session.as_str(),
            adapter.name(),
            repo,
            super::sessions::Verb::Exec,
        )
        .with_stable_short(&registry_short)
        .with_safety_policy_sha256(safety_policy_sha256)
        // Issue #169: `exec::run_with` has no `PromptRole` parameter to get
        // wrong (see `agent.rs`'s own module doc comment: a delegated run is
        // always a worker session), so this is always `Worker` -- an
        // accurate reflection of what every headless delegation actually
        // runs as today.
        .with_role(super::prompt::PromptRole::Worker.label()),
    );

    // Item 10: owned across every cycle of the loop below (the pre-flight
    // check and, on a usage-limit park, the second call further down), so
    // the no-usage-source blind-delay line and `PacingBlind` announce once
    // for the whole run rather than once per restart.
    let mut pace_flags = pace::PaceGateFlags::default();
    let http_poller = super::poll::HttpPoller::new(cfg.chrome.events);
    // Issue #243 (review round, F3): owned across every cycle too, so a
    // screening summary that has not changed since the last poll is
    // announced once for the whole run, not once per restart.
    let mut screening_announced: Option<String> = None;
    let mut compact_budget = CompactBudget::default();
    let compact_window = Duration::from_secs(cfg.supervise.interval_secs);
    // Issue #358 (T9): true for exactly the first trip through this loop --
    // the pre-launch call that decides whether a brand-new worker gets to
    // start at all. Cleared unconditionally right after that first call, so
    // every later trip (an ordinary restart, a nudge restart, an in-place
    // compact-continue) paces normally instead of being read as another
    // fresh launch.
    //
    // Issue #358 review finding #7: seeded from `initial_launch_allowed`,
    // not hardcoded -- a recursive re-entry of this same function (a
    // provider-switch harness-handover restart) passes `false`, since that
    // is never this delegation's own first launch even though it is this
    // CALL FRAME's first loop iteration.
    let mut initial_launch = initial_launch_allowed;

    loop {
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "exec",
            session.as_str(),
            now_fn,
            sleep_fn,
            Some(&announcer),
            adapter.provider_for_model(execution_model.as_deref()),
            pace::PaceGate {
                use_credits: cfg
                    .pace
                    .use_credits
                    .for_provider(adapter.provider_for_model(execution_model.as_deref())),
                poller: cfg
                    .pace
                    .poll_enabled
                    .then_some(&http_poller as &dyn super::poll::UsagePoller),
                initial_launch,
            },
            &mut pace_flags,
        );
        initial_launch = false;

        // P2/P3: `_child_guard` holds this cycle's child in the console-close
        // pid registry and in a kill-on-close job for as long as it is in
        // scope -- which is this loop iteration, i.e. exactly the child's own
        // life. Dropped (and so released) at the end of the iteration, after
        // the child has been reaped, and again by every arm that returns.
        let (mut child, tap, _child_guard) = supervise::spawn_tapped(command, stdin_prompt.clone())
            .map_err(|error| {
                adapters::format_launch_error(error.as_ref(), adapter.name(), adapter.program())
            })?;
        // Issue #281: this cycle's own work is now in flight -- cleared by
        // `supervise_run`'s tick closure the instant it sees a turn signal
        // for THIS session. Turn `0`: no turn signal has landed yet for this
        // fresh child, so if it crashes before its first one, `0` honestly
        // says nothing was confirmed complete.
        session_guard.stamp_in_flight(super::sessions::Verb::Exec.as_str(), 0);
        // Item 3: the messages folded into the launch prompt are consumed
        // here, right after the spawn that actually carried them has
        // genuinely started -- not before pacing or the spawn itself, where
        // a park or a failed launch would have moved them to `read/` with no
        // session ever having seen them. Drains to empty on the first
        // successful spawn, so a later restart's own iteration through this
        // same loop finds nothing left to consume and is a no-op. A failed
        // consume must not fail the launch itself -- best effort, like the
        // rest of state-dir housekeeping -- since the mail has already
        // reached the prompt either way.
        for (path, _) in mail_entries.drain(..) {
            let _ = super::mail::consume_and_log(
                &state,
                &mail_slug,
                &path,
                &registry_short,
                "exec",
                "exec:launch-prompt",
            );
        }
        // Fresh scorer per iteration, over the current session's transcript.
        let mut scorer = score::IncrementalScorer::new(transcript.clone());
        let mut rotted = false;
        let mut compact_requested = false;
        let mut limit_hit = false;
        let mut limit_confirmation_detail = None;
        let mut nudged_by: Option<String> = None;
        // Issue #155, Phase 5(d): fresh per iteration too -- the child a
        // restart mints is a fresh transcript, so its own soft-warn latch and
        // exhaustion flag start over along with it (see `worker_budget`'s own
        // doc comment for the scope this implies).
        let mut budget_soft_warned = false;
        let mut budget_exhausted = false;

        // C3: reset below whenever this run reported a turn of its own.
        let mut progressed = false;
        // Issue #310 (3a): fresh per iteration, like `rotted`/`limit_hit`
        // above -- a restart mints a fresh child, so its own stall clock
        // starts over along with it.
        let mut stalled = false;
        let mut capacity_pattern = None;
        let mut account_pattern = None;
        // Bound to a local so `transcript_derived` can hand the tick either
        // this resolver or nothing at all (review round 2, S1).
        let derive = || derive_transcript(&session);
        let outcome = supervise_run(
            &mut child,
            Instant::now() + timeout,
            poll,
            &mut scorer,
            adapter.as_ref(),
            &cfg.score,
            &cfg.pace,
            &cfg.screen.thresholds(),
            &cfg.fallback.effective_health(),
            &state,
            server.as_ref(),
            session.as_str(),
            &registry_short,
            &mut session_guard,
            &announcer,
            &mut screening_announced,
            &mut rotted,
            &mut compact_requested,
            &mut compact_budget,
            compact_window,
            &mut progressed,
            &tap,
            &mut limit_hit,
            &mut capacity_pattern,
            &mut account_pattern,
            &mut limit_confirmation_detail,
            &mut nudged_by,
            nudge_restarts,
            cfg.supervise.max_nudges,
            can_restart,
            &mut transcript,
            transcript_derived.then_some(&derive as &dyn Fn() -> PathBuf),
            worker_budget,
            &prior_usage,
            prior_tool_calls,
            &mut budget_soft_warned,
            &mut budget_exhausted,
            repo,
            &cfg,
            Duration::from_secs(cfg.supervise.idle_no_tool_secs),
            Duration::from_secs(cfg.supervise.in_tool_secs),
            Duration::from_secs(cfg.supervise.stall_grace_secs),
            &mut stalled,
            args.cancellation.as_deref(),
        )?;

        if args
            .cancellation
            .as_ref()
            .is_some_and(|flag| super::provider::adapter::Cancellation::is_cancelled(flag.as_ref()))
        {
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(130);
        }

        if budget_exhausted {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "budget",
                    score: 0,
                    action: "kill",
                    detail: &transcript.display().to_string(),
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: token/tool-call budget exhausted, stopping (exit \
                 {EXIT_BUDGET_EXHAUSTED})"
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(EXIT_BUDGET_EXHAUSTED);
        }

        // C3: the budget is *consecutive* nudge restarts, which is what
        // `[supervise] max_nudges` has always been documented as. It was
        // implemented cumulatively -- never reset -- so a long-lived session
        // that was nudged three times over an hour, doing useful work in
        // between each, permanently lost the ability to be nudged again.
        // A turn boundary reported by this session is the evidence that it
        // got somewhere, so the run of consecutive nudges is over.
        nudge_restarts = nudges_after(nudge_restarts, progressed);

        // `supervise_child` checks the child's exit status before calling the
        // tick, so a fast limit-hit exit (print the notice, exit immediately,
        // exactly what a real exhausted-window run looks like) can race past
        // the last tick that would have caught it. A final drain here closes
        // that race without touching supervise_child's general contract --
        // `drain_to_eof`, not `try_lines`, because `try_lines` alone is just
        // as non-blocking as every tick's own call and can still lose the
        // race it looks like it closes (root-caused via a deterministic
        // repro in `supervise.rs`'s own test module, not by inspection alone).
        // Issue #227: a provider capacity/overload error and an account/
        // billing exhaustion are both text-tail conditions, exactly like a
        // vendor usage-limit message. T4 (C-4): they are now ALSO scanned in
        // the tick, because the tick's own `tap.try_lines()` is destructive --
        // a capacity line printed more than one poll before the exit never
        // reached this final drain at all, and the run was misclassified as a
        // timeout/crash and restarted with no backoff. The two readings are
        // merged here rather than replacing one another. A vendor-confirmed
        // usage-limit message still wins the classification outright (both
        // labels are dropped below when `limit_hit` holds), and `account_
        // pattern` still wins over `capacity_pattern` -- burning the restart
        // budget on a capacity retry when the account itself is empty would
        // just fail again immediately.
        if limit_hit {
            capacity_pattern = None;
            account_pattern = None;
        } else {
            let final_lines = tap.drain_to_eof(supervise::FINAL_DRAIN_BUDGET);
            // Round 4 bug 4a: a `--output-format json` result's own
            // `modelUsage.<model>.contextWindow` is the real window for the
            // model that actually ran -- learned here, once, the moment it
            // is seen, rather than trusting the catalogue's possibly-stale
            // number forever. Best-effort and silent: most launches print
            // no such result at all (interactive sessions, `--output-format
            // text`), which reads as "nothing observed", never an error.
            if let Some((model_id, window)) =
                super::model_window::parse_observed_window(&final_lines.join("\n"))
                && let Ok(home) = crate::utils::home_dir()
            {
                super::model_window::record(
                    &home,
                    &[model_id.as_str(), execution_model.as_deref().unwrap_or("")],
                    window,
                );
            }
            let limit_text_seen = pace::scan_for_limit(
                &final_lines,
                &state,
                session.as_str(),
                "exec",
                &mut std::io::stderr(),
            );
            if limit_text_seen {
                let now = now_fn();
                match pace::confirm_limit_hit(
                    &state,
                    &cfg.pace,
                    now,
                    adapter.provider_for_model(execution_model.as_deref()),
                ) {
                    pace::LimitConfirmation::Confirmed { detail } => {
                        limit_hit = true;
                        limit_confirmation_detail = Some(detail);
                    }
                    pace::LimitConfirmation::Unconfirmed { detail } => {
                        pace::note_unconfirmed_limit_text(
                            &state,
                            now,
                            session.as_str(),
                            "exec",
                            &detail,
                            &mut std::io::stderr(),
                        );
                    }
                }
            }
            if !limit_hit {
                account_pattern =
                    account_pattern.or_else(|| pace::scan_for_account_exhausted(&final_lines));
                if account_pattern.is_none() {
                    capacity_pattern =
                        capacity_pattern.or_else(|| pace::scan_for_capacity_error(&final_lines));
                }
            }
        }

        // Issue #227: an account/billing exhaustion is a hard, non-retryable
        // condition -- unlike a usage window (which resets on its own) or a
        // capacity error (which is worth retrying), restarting cannot fix an
        // empty account, so this gives up immediately without spending any
        // of the restart budget. Gated on a genuinely non-zero exit: a clean
        // exit with incidental matching text (vanishingly unlikely given how
        // specific these phrases are) must still read as success.
        if let Some(label) = account_pattern
            && matches!(outcome, Outcome::Exited(code) if code != 0)
        {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "account",
                    score: 0,
                    action: "account-exhausted",
                    detail: label,
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: {} account exhausted ({label}); this is not retryable -- \
                 check billing before restarting (exit {EXIT_ACCOUNT_EXHAUSTED})",
                adapter.name()
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(EXIT_ACCOUNT_EXHAUSTED);
        }

        if should_attempt_compact(compact_requested, limit_hit) {
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            // Issue #798 (`[jev] compaction_select`): best-effort, off by
            // default -- `compaction_focus_for_transcript` checks the gate
            // and credential BEFORE touching the transcript at all (review
            // of 6bdd7675, defect #1), so this costs nothing observable on
            // that (today's default) path.
            let compact_focus = handoff::compaction_focus_for_transcript(
                &cfg,
                &state,
                adapter.as_ref(),
                Some(&transcript),
                cfg.handoff.tail_items,
                supervise::COMPACT_FOCUS,
            );
            let compact_result = compact_in_place(
                adapter.as_ref(),
                Some(&transcript),
                Duration::from_millis(cfg.supervise.compact_timeout_ms),
                poll,
                &compact_focus,
                |compact_prompt| {
                    let session_ref = SessionRef {
                        id: session.clone(),
                        cwd: repo.to_path_buf(),
                    };
                    let (mut compact, stdin_prompt) = headless_resume_launch(
                        adapter.as_ref(),
                        compact_prompt,
                        &session_ref,
                        &extra,
                        prompt_via_stdin,
                    )?;
                    compact.current_dir(repo);
                    apply_headless_cost_levers(
                        &mut compact,
                        &cfg,
                        adapter.name(),
                        Some(compact_prompt),
                        &state,
                        &session,
                    );
                    apply_session_env(&mut compact, &session);
                    Some((compact, stdin_prompt))
                },
            )
            .and_then(|()| {
                let prompt_text = prompt
                    .as_deref()
                    .ok_or_else(|| "no prompt available for continuation".to_string())?;
                let continuation = protect_compaction_continuation(&state, repo, &cfg, prompt_text)
                    .map_err(|error| format!("sensitive-data masking failed: {error}"))?;
                let session_ref = SessionRef {
                    id: session.clone(),
                    cwd: repo.to_path_buf(),
                };
                let (mut command, stdin_prompt) = headless_resume_launch(
                    adapter.as_ref(),
                    &continuation,
                    &session_ref,
                    &extra,
                    prompt_via_stdin,
                )
                .ok_or_else(|| {
                    format!(
                        "adapter '{}' cannot resume a headless session in place",
                        adapter.name()
                    )
                })?;
                command.current_dir(repo);
                apply_headless_cost_levers(
                    &mut command,
                    &cfg,
                    adapter.name(),
                    Some(&continuation),
                    &state,
                    &session,
                );
                apply_session_env(&mut command, &session);
                Ok((command, stdin_prompt))
            });

            let verified = compact_result.is_ok();
            announcer.emit(&super::announce::Event::Compact { verified });
            match compact_result {
                Ok((continued, continued_stdin)) => {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "compact",
                            score: 0,
                            action: "compact",
                            detail: &transcript.display().to_string(),
                            observed_at: None,
                        },
                    );
                    command = continued;
                    stdin_prompt = continued_stdin;
                    continue;
                }
                Err(reason) => {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "compact",
                            score: 0,
                            action: "compact-failed",
                            detail: &reason,
                            observed_at: None,
                        },
                    );
                    writeln!(
                        w,
                        "zirv ctx exec: {reason}; falling back to restart with handoff"
                    )?;
                    rotted = true;
                }
            }
        }

        match outcome {
            Outcome::Exited(code) if !(limit_hit || capacity_pattern.is_some() && code != 0) => {
                // Issue #37: a clean session end -- no rot, no timeout, no
                // restart -- previously never harvested at all. Gated on
                // `cfg.memory.harvest` here too, before the transcript is
                // even read, so an operator who left harvesting off never
                // pays for the read or the distiller call this seam can
                // make. Best-effort, discarded via `let _ =`: a harvest
                // failure must never turn a successful exit into a failed
                // one.
                if cfg.memory.enabled && cfg.memory.harvest {
                    let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
                    let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
                    let _ = super::memory::harvest_at_session_end(
                        adapter.as_ref(),
                        &distiller_model,
                        &ctx,
                        Duration::from_secs(cfg.handoff.timeout_secs),
                        repo,
                        &state,
                        &mail_slug,
                        &cfg,
                    );
                }
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );
                // Issue #349: the child is genuinely gone -- no rot, no
                // timeout, no restart -- so `Supervisor` is the authority
                // that actually knows this, regardless of what any
                // `AdapterHook` observation last said about the (now
                // nonexistent) turn.
                let _ = super::attention::record(
                    &state,
                    &registry_short,
                    super::attention::Observation::new(
                        super::attention::Authority::Supervisor,
                        format!("exited with code {code}"),
                        100,
                        now_secs(),
                    )
                    .with_lifecycle(super::attention::Lifecycle::Exited),
                    now_secs(),
                );
                session_guard.release();
                return Ok(code);
            }
            Outcome::Exited(_) | Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        // N4: a nudge relaunch is neither a limit park nor a rot restart --
        // `supervise_run`'s own tick only ever sets `nudged` when a relaunch
        // is actually possible (a known prompt) and under the consecutive
        // cap, so this arm always follows through rather than needing its
        // own "no prompt"/"over budget" fallbacks the way rot's restart does.
        if let Some(nudged_from) = nudged_by.take() {
            // T4 (C-6): unreachable by construction (`supervise_run` only ever
            // sets `nudged` when a prompt is known), but a hot restart path
            // must not carry an `expect` -- the release profile is
            // `panic = "abort"`, so a wrong assumption here would kill the
            // supervised session rather than ending the run honestly.
            let prompt_text = prompt
                .clone()
                .ok_or_else(|| "cannot relaunch after a nudge: no prompt is known".to_string())?;

            let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
            let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
            let previous = handoff::latest_for_repo(&state, repo)
                .ok()
                .flatten()
                .map(|(_, h)| h);
            let (note, source) = handoff::distill_or_structural(
                adapter.as_ref(),
                &distiller_model,
                &ctx,
                Duration::from_secs(cfg.handoff.timeout_secs),
                cfg.chrome.events,
                previous.as_ref(),
            );
            let stored = handoff::store(&state, repo, session.as_str(), &note)?;

            // Harvest every outgoing child even for an unbounded run: the same
            // accumulator now feeds both budget enforcement and delegation
            // accounting, so skipping it would hide pre-handoff/restart spend.
            harvest_spend(
                adapter.as_ref(),
                &transcript,
                &mut prior_usage,
                &mut prior_tool_calls,
            );
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);
            transcript_derived = true;

            // Issue #285: advances and persists the durable objective (if
            // any) against the spend just harvested, purely for the side
            // effect -- the `compile::compile` call right below reloads the
            // SAME record fresh from disk, so it already carries the updated
            // counters. No separate raw-text injection needed here, unlike
            // the rot/timeout restart further down: this branch recomposes.
            let _ = objective_layer_for_restart(
                &state,
                repo,
                now_fn(),
                agent::token_spend(&prior_usage),
            );

            // Recompose fresh -- unlike an ordinary restart, which reuses
            // the launch-time `composed`/`prompt_args` untouched (see this
            // module's own doc comment), a nudge relaunch is explicitly the
            // chance to pick up what prompted it: the nudge's own payload
            // arrived as ordinary session-addressed mail (`sessions::run_
            // nudge_with` stores it before writing the wake-up marker), so
            // re-listing mail for the session that was just nudged and
            // folding it in through `with_mail_layer` delivers it with zero
            // new injection machinery.
            //
            // Issue #44: goes through `compile::compile` a second time here,
            // same as the launch-time call above. One small, deliberate
            // behavior refinement over the pre-compiler code this replaces:
            // `compile` re-reads the memory bank fresh (it is a pure function
            // of `state` at call time) rather than reusing the launch-time
            // `memory_entries` snapshot the old duplicated call passed in
            // verbatim. The bank is repo-wide and does not go stale *within*
            // one `run_with` call either way (see the removed comment this
            // replaced), so this is not a correctness change -- a nudge that
            // lands after something new was remembered now picks it up
            // instead of seeing the launch-time snapshot.
            let mut fresh_compiled = super::compile::compile(
                crate::utils::home_dir().ok().as_deref(),
                repo,
                skip_injection,
                &cfg,
                adapter.as_ref(),
                super::prompt::PromptRole::Worker,
                &state,
                now_secs(),
                super::adapters::LaunchMode::Headless,
                true,
            );
            if let Some(task) = prompt.as_deref() {
                super::compile::select_skill_descriptions_for_task(
                    &mut fresh_compiled,
                    &cfg,
                    &state,
                    repo,
                    crate::utils::home_dir().ok().as_deref(),
                    task,
                );
            }
            let mut fresh = fresh_compiled.composed;
            // C7: `registry_short`, not `short_id(session)` -- `session`
            // has just been rotated above, and the nudge's own payload was
            // addressed to the stable registry address the sender resolved.
            //
            // N5: gated on `fresh.is_some()` as well as `cfg.mail.enabled`,
            // exactly like the launch path's `composed.is_some()` gate.
            // Under `--simple` there is no composed prompt for `with_mail_
            // layer` to fold mail into either, so listing it here only led
            // to it being consumed (moved to `read/`) by the post-spawn
            // drain below -- silently marking a message read that no
            // session ever saw.
            //
            // Medium 3: `|| !system_prompt_supported` is the same escape the
            // launch path's own gate (~401) has -- without it, `--simple`
            // makes `fresh` always `None` regardless of adapter, so a codex
            // run under `--simple` dropped the nudge's own guidance
            // silently while still spending a `max_nudges` slot on the
            // restart it triggered. Codex's real channel here is the task
            // prompt text (`task_prompt_with_mail_fallback` below), which
            // does not depend on `fresh`/`composed` existing at all.
            //
            // Final wave item 2: deliberately NOT also gated on the launch-
            // time `mail_deliverable` (`adapter_builds_launch ||
            // system_prompt_supported`) the way it used to be. That flag
            // answers "can *this launch's own argv shape* carry a fallback"
            // -- true only for a zirv-built launch, since an explicit `--
            // command` is the caller's fixed argv with nothing of zirv's own
            // to append to. A nudge restart is not that launch: every
            // relaunch arm (nudge, park, rot/timeout) rebuilds through
            // `build_headless`, which is *always* the adapter's own launch,
            // regardless of what the original invocation looked like. So by
            // the time this code runs, the task-prompt-text channel exists
            // unconditionally -- reusing the original launch's `mail_
            // deliverable` here understated what a relaunch can actually
            // deliver.
            let nudge_mail: Vec<(PathBuf, super::mail::Message)> = if cfg.mail.enabled {
                // Read back off the guard, which is the one thing that
                // demonstrably did not rotate when `refresh_session` ran
                // a few lines above.
                super::mail::list(
                    &state,
                    &mail_slug,
                    Some(adapter.name()),
                    super::sessions::delivery_filter(Some(session_guard.short()), &registry_short),
                )
                .unwrap_or_default()
            } else {
                Vec::new()
            };
            let nudge_mail_msgs: Vec<super::mail::Message> =
                nudge_mail.iter().map(|(_, msg)| msg.clone()).collect();
            if !nudge_mail_msgs.is_empty() {
                announcer.emit(&super::announce::Event::MailDelivered {
                    count: nudge_mail_msgs.len(),
                });
            }
            // Folded into `composed` only for an adapter with a real
            // injection mechanism: `injection_args_for_session` always
            // turns `composed` into an empty argv for one without, so
            // folding mail in here only would tag it `PromptSource::Mail`
            // on a prompt nobody ever receives -- the fallback below
            // (`task_prompt_with_mail_fallback`, which already gates on
            // this same flag internally) is that adapter's one real
            // channel.
            fresh = if relaunch_system_prompt_supported {
                super::prompt::with_mail_layer(
                    fresh,
                    &nudge_mail_msgs,
                    cfg.mail.max_delivered_bytes,
                    parent_short.as_deref(),
                )
            } else {
                fresh
            };
            // PLAUSIBLE-1: re-apply the adapter layer and the operator's own
            // command-line instruction from the text captured at launch.
            // `launch_command` is the cleaned argv, so merging against it
            // again would find no flag and drop the instruction entirely.
            let fresh = super::prompt::relayer_recomposed(
                adapter.as_ref(),
                fresh,
                operator_prompt_text.as_deref(),
                super::prompt::PromptRole::Worker,
                &cfg.prompt,
            );
            composed = super::obfuscate_store::protect_composed(
                &state,
                repo,
                &cfg,
                fresh,
                "exec_system_prompt",
            )?;
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            )?;
            // Folded into the prompt above, but only actually marked read
            // once the relaunch that carries it genuinely spawns -- the same
            // Item 3 discipline every other delivery seam in this function
            // follows.
            mail_entries = nudge_mail;
            // Medium 4: kept in lockstep with `mail_entries` just above --
            // a later park or rot-restart's own `task_prompt_with_mail_
            // fallback` call reuses `mail_messages` verbatim rather than
            // re-listing (see those arms' own comments), so leaving this
            // holding the stale launch-time list would have re-appended
            // already-consumed mail on that later restart while silently
            // dropping the nudge's own guidance from it entirely.
            mail_messages = nudge_mail_msgs.clone();

            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                relaunch_system_prompt_supported,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                relaunch_system_prompt_supported,
            ));
            announcer.emit(&super::announce::Event::Nudge {
                from: nudged_from,
                disposition: super::announce::NudgeDisposition::Relaunching,
            });

            nudge_restarts += 1;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-restart",
                    detail: &format!("{source} handoff at {}", stored.display()),
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: nudged ({nudge_restarts}/{}), restarting with a {source} handoff",
                cfg.supervise.max_nudges
            )?;

            let combined = format!(
                "{prompt_text}\n\n{}",
                handoff::labeled_for_injection(&note, &cfg.screen.thresholds())
            );
            let combined = super::prompt::task_prompt_with_composed_fallback(
                &combined,
                relaunch_system_prompt_supported,
                composed.as_ref(),
            );
            let mail_in_composed = composed
                .as_ref()
                .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
            // A nudge relaunch re-lists mail fresh (`nudge_mail_msgs` above),
            // so the fallback for an uninjectable adapter has to use that
            // same fresh listing, not the launch-time `mail_messages`.
            let combined = super::prompt::task_prompt_with_mail_fallback(
                &combined,
                (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
                &nudge_mail_msgs,
                cfg.mail.max_delivered_bytes,
                parent_short.as_deref(),
            );
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            let (mut rebuilt, sp) = build_headless(&combined, &session, &extra)?;
            rebuilt.current_dir(repo);
            apply_session_env(&mut rebuilt, &session);
            command = rebuilt;
            stdin_prompt = sp;
            continue;
        }

        if limit_hit {
            // Issue #186: a vendor-confirmed block is the only point where a
            // running session may move harnesses. The child is already stopped,
            // so this never interrupts an in-flight response. Only launches
            // zirv itself built from prompt data are portable across vendors;
            // an operator-owned explicit command keeps today's park behavior.
            let visited: Vec<String> = env(super::fallback::VISITED_ENV)
                .map(|raw| super::config::split_csv_list(&raw))
                .unwrap_or_default();
            let source_model = adapters::last_model_flag(&args.command);
            let delegation = env(super::fallback::DELEGATION_ENV);
            let is_delegation = delegation.is_some();
            let route_request = super::fallback::RouteRequest {
                requested: adapter.name(),
                source_model,
                source_model_explicit: if is_delegation {
                    delegation.as_deref() == Some("explicit-model")
                } else {
                    source_model.is_some()
                },
                delegation: is_delegation,
                bounds: super::fallback::TaskBounds {
                    tokens: worker_budget.tokens,
                    tool_calls: worker_budget.tool_calls,
                },
                now: now_fn(),
                // A running worker's own vendor-blocked reroute is not an
                // orchestrator-seat delegation (issue #328's exclusion is
                // scoped to `agent::run_with` specifically).
                exclude: &[],
                // This very session is the one being rerouted, so its own
                // registry row is not competing capacity.
                requester: Some(session.as_str()),
            };
            let route = (adapter_builds_launch && prompt.is_some())
                .then(|| {
                    super::fallback::route_blocked_session(&state, &cfg, route_request, &visited)
                })
                .flatten();
            let deferred_reset = (route.is_none() && adapter_builds_launch && prompt.is_some())
                .then(|| {
                    super::fallback::earliest_reset_choice(&state, &cfg, route_request, &visited)
                })
                .flatten();
            let alternate = route
                .as_ref()
                .map(|route| {
                    (
                        route.selected.clone(),
                        route.model.clone(),
                        route.detail(super::pace::Seat::Cli),
                    )
                })
                .or_else(|| {
                    deferred_reset.as_ref().and_then(|choice| {
                        if !choice.is_cross_harness() {
                            return None;
                        }
                        Some((
                            choice.selected.clone(),
                            choice.model.clone()?,
                            choice.detail(),
                        ))
                    })
                });
            let route_observed_at = route.as_ref().and_then(|route| route.requested_observed_at);

            if let Some((selected_agent, selected_model, selection_detail)) = alternate {
                let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
                let previous = handoff::latest_for_repo(&state, repo)
                    .ok()
                    .flatten()
                    .map(|(_, h)| h);
                let (note, source) = handoff::distill_or_structural(
                    adapter.as_ref(),
                    &distiller_model,
                    &ctx,
                    Duration::from_secs(cfg.handoff.timeout_secs),
                    cfg.chrome.events,
                    previous.as_ref(),
                );
                let stored = handoff::store(&state, repo, session.as_str(), &note)?;

                // The source-harness portion is complete at this boundary.
                // Record it before harvesting the current transcript into the
                // budget accumulator, otherwise the helper would count it twice.
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );

                // Preserve both accounting and any configured delegation budget
                // across the vendor boundary. This includes the just-stopped
                // child plus every prior restart already accumulated here.
                harvest_spend(
                    adapter.as_ref(),
                    &transcript,
                    &mut prior_usage,
                    &mut prior_tool_calls,
                );
                let spent_tokens = prior_usage
                    .context_total()
                    .saturating_add(prior_usage.output_tokens);
                let remaining_tokens = worker_budget
                    .tokens
                    .map(|limit| limit.saturating_sub(spent_tokens));
                let remaining_tool_calls = worker_budget
                    .tool_calls
                    .map(|limit| limit.saturating_sub(prior_tool_calls));
                if worker_budget
                    .tokens
                    .is_some_and(|_| remaining_tokens == Some(0))
                    || worker_budget
                        .tool_calls
                        .is_some_and(|_| remaining_tool_calls == Some(0))
                {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "budget",
                            score: 0,
                            action: "fallback-budget-exhausted",
                            detail: "usage limit coincided with the delegation budget ceiling",
                            observed_at: None,
                        },
                    );
                    session_guard.release();
                    return Ok(EXIT_BUDGET_EXHAUSTED);
                }

                // Issue #285: side effect only -- the nested `run_with_clock_
                // inner` call below starts its own launch, which reloads
                // this SAME durable objective (keyed by repository, not by
                // these args) fresh via its own first `compile::compile`
                // call.
                let _ = objective_layer_for_restart(&state, repo, now_fn(), spent_tokens);

                let confirmation = limit_confirmation_detail
                    .as_deref()
                    .map(|detail| format!("; structured confirmation: {detail}"))
                    .unwrap_or_default();
                let detail = format!(
                    "{}{confirmation}; {} handoff at {}",
                    selection_detail,
                    source,
                    stored.display()
                );
                let _ = log::append(
                    &state,
                    &log::Decision {
                        ts: now_secs(),
                        session: session.as_str(),
                        verb: "exec",
                        verdict: "limit",
                        score: 100,
                        action: "harness-handover",
                        detail: &detail,
                        observed_at: route_observed_at,
                    },
                );
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit; continuing on another harness ({detail})"
                )?;

                // T4 (C-6): same reasoning as the nudge relaunch above --
                // routing is only ever reached with a resolved prompt, and a
                // hot restart path must end the run honestly rather than
                // abort the process on a broken assumption.
                let prompt_text = prompt.clone().ok_or_else(|| {
                    "cannot continue on another harness: no prompt is known".to_string()
                })?;
                let continuation = format!(
                    "{prompt_text}\n\nThe previous harness exhausted its usage window. Continue from this handoff without redoing completed work:\n\n{}",
                    handoff::labeled_for_injection(&note, &cfg.screen.thresholds())
                );
                let target = adapters::select(Some(&selected_agent), &[], &cfg)?;
                // Issue #358 (task T3): this run's own token reservation, if
                // any (`args.reservation_id`, set only when this is a
                // delegated worker's launch -- see `ExecArgs::reservation_id`'s
                // own doc comment), moves providers right here along with the
                // harness itself: released against the OLD provider and
                // re-reserved against the NEW one for the remaining token
                // ceiling, so the per-provider ledger never keeps counting
                // outstanding spend against a harness this run has already
                // left. `None` when this run carries no reservation to begin
                // with (a plain `zirv ctx exec` with no delegation) -- never
                // minting one here that nothing downstream would ever settle.
                // Best-effort, matching every other reservation write in this
                // codebase: a ledger error must never block an otherwise-
                // legitimate harness handover.
                let mut reservation_id = None;
                if let Some(old_id) = args.reservation_id.as_deref() {
                    let _ = super::reservation::release(
                        &state,
                        adapter.provider_for_model(execution_model.as_deref()),
                        old_id,
                    );
                    let target_provider = target.provider_for_model(Some(selected_model.as_str()));
                    reservation_id = match super::reservation::reserve(
                        &state,
                        target_provider,
                        session.as_str(),
                        remaining_tokens.unwrap_or(0),
                        now_fn(),
                    ) {
                        Ok(reservation) => Some(reservation.id),
                        Err(e) => {
                            eprintln!(
                                "zirv ctx exec: failed to record a token reservation for \
                                 provider '{target_provider}': {e}"
                            );
                            None
                        }
                    };
                    // Finding #4 (issue #358 review): surface exactly which
                    // ledger this delegation's reservation now lives on, so
                    // whichever caller settles once the whole recursive
                    // handover chain returns settles the right one -- not
                    // the provider (and id) it started this run on.
                    report.final_reservation =
                        reservation_id.clone().map(|id| (id, target_provider));
                }
                let nested_args = ExecArgs {
                    agent: Some(selected_agent.clone()),
                    session_id: None,
                    transcript: None,
                    prompt: Some(continuation),
                    max_restarts: args.max_restarts,
                    timeout_secs: args.timeout_secs,
                    budget_tokens: remaining_tokens,
                    max_tool_calls: remaining_tool_calls,
                    // Not re-set: the objective is durable per repository
                    // (keyed by `state::repo_slug`, not by these args), so
                    // the nested `run_with_clock_inner` call picks up the
                    // same record via `compile::compile` on its own. Setting
                    // it again here would reset `spent_tokens` to zero.
                    objective: None,
                    command: target.model_args(&selected_model),
                    simple: args.simple,
                    reservation_id,
                    ..Default::default()
                };

                let mut next_visited = visited;
                if !next_visited.iter().any(|name| name == adapter.name()) {
                    next_visited.push(adapter.name().to_string());
                }
                let visited_csv = next_visited.join(",");
                let nested_env = |key: &str| {
                    if key == super::fallback::VISITED_ENV {
                        Some(visited_csv.clone())
                    } else {
                        env(key)
                    }
                };

                // The old session has ended and its handoff is durable before
                // the continuation is registered. Releasing first prevents two
                // live registry entries from claiming one logical worker.
                session_guard.release();
                return run_with_clock_inner(
                    &nested_args,
                    w,
                    repo,
                    &nested_env,
                    now_fn,
                    sleep_fn,
                    Some(&registry_short),
                    false,
                    report,
                    present,
                );
            }

            let mut wait_detail = deferred_reset
                .as_ref()
                .map(super::fallback::ResetChoice::detail)
                .unwrap_or_else(|| {
                    "agent reported a usage limit; parking until the current window resets"
                        .to_string()
                });
            if let Some(detail) = &limit_confirmation_detail {
                wait_detail.push_str(&format!("; structured confirmation: {detail}"));
            }
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "limit",
                    score: 100,
                    action: "limit-park",
                    detail: &wait_detail,
                    observed_at: None,
                },
            );
            writeln!(w, "zirv ctx exec: {wait_detail}")?;

            // A confirmed vendor refusal is authoritative even when the
            // operator disabled proactive pacing. Re-enable only this park;
            // otherwise `wait_for_window` would return immediately and launch
            // straight back into the refusal it just confirmed.
            let mut confirmed_limit_pace = cfg.pace.clone();
            confirmed_limit_pace.enabled = true;
            pace::wait_for_window(
                w,
                &state,
                &confirmed_limit_pace,
                "exec",
                session.as_str(),
                now_fn,
                sleep_fn,
                Some(&announcer),
                adapter.provider_for_model(execution_model.as_deref()),
                pace::PaceGate {
                    // A vendor-reported limit hit parks even with use_credits
                    // enabled: the vendor limiting us means credits are
                    // exhausted or not actually enabled plan-side, and an
                    // immediate relaunch would just re-hit it.
                    use_credits: false,
                    poller: cfg
                        .pace
                        .poll_enabled
                        .then_some(&http_poller as &dyn super::poll::UsagePoller),
                    // A confirmed vendor refusal on a session already
                    // running is exactly the mid-run pacing T9 leaves
                    // intact -- never the pre-launch call that never blocks.
                    initial_launch: false,
                },
                &mut pace_flags,
            );

            let Some(prompt_text) = prompt.clone() else {
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit and the original prompt is unknown, so it cannot relaunch. Pass --prompt to enable parking."
                )?;
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );
                session_guard.release();
                return Ok(EXIT_ROT_EXHAUSTED);
            };

            // A park mints a fresh transcript exactly like a restart, so the
            // outgoing child's spend must be harvested for both whole-run
            // accounting and any configured token/tool-call ceiling.
            harvest_spend(
                adapter.as_ref(),
                &transcript,
                &mut prior_usage,
                &mut prior_tool_calls,
            );
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);
            transcript_derived = true;
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            )?;
            // M2: a park mints a new session id, just like a restart, so the
            // injection attribution is re-logged under it rather than only
            // ever naming the first session this run started with.
            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                relaunch_system_prompt_supported,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                relaunch_system_prompt_supported,
            ));
            // M8: the user's own extra flags survive the relaunch too, not
            // just zirv's own (the system prompt args, and now the
            // sandbox/policy prepend).
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            let prompt_text = super::prompt::task_prompt_with_composed_fallback(
                &prompt_text,
                relaunch_system_prompt_supported,
                composed.as_ref(),
            );
            let mail_in_composed = composed
                .as_ref()
                .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
            // A park does not itself re-list mail (matching every other
            // value it reuses here), so the fallback for an uninjectable
            // adapter reuses whatever `mail_messages` currently holds --
            // the launch-time listing, or a nudge's own fresher one if this
            // run was nudged before it parked (Medium 4: `mail_messages` is
            // kept in lockstep with `mail_entries` at the one place that
            // reassigns it).
            let prompt_text = super::prompt::task_prompt_with_mail_fallback(
                &prompt_text,
                (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
                &mail_messages,
                cfg.mail.max_delivered_bytes,
                parent_short.as_deref(),
            );
            let (mut rebuilt, sp) = build_headless(&prompt_text, &session, &extra)?;
            rebuilt.current_dir(repo);
            apply_session_env(&mut rebuilt, &session);
            command = rebuilt;
            stdin_prompt = sp;
            continue;
        }

        // Issue #227: only a genuinely non-zero `Outcome::Exited` counts --
        // the match arm above already excludes a capacity-flagged clean exit
        // from reaching here at all, and `TimedOut`/`StoppedByTick` are the
        // supervisor's own kill, never the child's own capacity-triggered
        // exit.
        let capacity_exit =
            capacity_pattern.is_some() && matches!(outcome, Outcome::Exited(code) if code != 0);

        let reason = if capacity_exit {
            "capacity"
        } else if stalled {
            "stalled"
        } else if rotted {
            "rot"
        } else {
            "timeout"
        };
        let exhausted_code = if capacity_exit {
            EXIT_CAPACITY_EXHAUSTED
        } else if stalled {
            EXIT_STALLED
        } else if rotted {
            EXIT_ROT_EXHAUSTED
        } else {
            EXIT_TIMEOUT
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "kill",
                detail: &transcript.display().to_string(),
                observed_at: None,
            },
        );

        let Some(prompt_text) = prompt.clone() else {
            writeln!(
                w,
                "zirv ctx exec: {reason} detected but the original prompt is unknown, so it cannot restart. Pass --prompt to enable restarts."
            )?;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "stand-down",
                    detail: "no prompt available for restart",
                    observed_at: None,
                },
            );
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        };

        // Issue #310 (3b): chain this boot across process boundaries BEFORE
        // deciding whether this process's own `max_restarts` allows another
        // one -- a tripped chain must give up even when this single
        // invocation's own budget still has room, since the whole point is
        // to catch a pattern that keeps recurring across separate
        // `exec`/`loop` launches, not just within one of them. A usage-limit
        // ("capacity") boot and a stall-detected one each get their own
        // class, so neither ever spends the plain `crash` budget "rot"/
        // "timeout" restarts do (issue #227).
        let failure_class = match reason {
            "capacity" => super::chain::FailureClass::UsageLimit,
            "stalled" => super::chain::FailureClass::Stalled,
            _ => super::chain::FailureClass::Crash,
        };
        let chain_key = super::state::repo_slug(repo);
        if let super::chain::ChainVerdict::Tripped { boots } =
            super::chain::record_boot_and_evaluate(
                &state,
                &chain_key,
                failure_class,
                false,
                now_secs(),
                cfg.supervise.chain_max_restarts,
                cfg.supervise.chain_max_gap_secs,
            )
        {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "chain-tripped",
                    detail: &format!(
                        "{boots} unplanned {reason} respawns within the configured gap; not \
                         auto-resuming"
                    ),
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: restart-chain breaker tripped ({boots} {reason} respawns within \
                 the configured gap); not auto-resuming -- run `zirv ctx status` (exit \
                 {exhausted_code})"
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        }

        if restarts >= max_restarts {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "give-up",
                    detail: "restart budget exhausted",
                    observed_at: None,
                },
            );
            if capacity_exit {
                let label = capacity_pattern.unwrap_or("provider capacity error");
                writeln!(
                    w,
                    "zirv ctx exec: {} finished: provider capacity limit ({label}) after \
                     {restarts} restarts; workspace changes are uncommitted (exit \
                     {exhausted_code})",
                    adapter.name()
                )?;
            } else {
                writeln!(
                    w,
                    "zirv ctx exec: {reason} after {restarts} restarts, giving up with exit {exhausted_code}"
                )?;
            }
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        }

        let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
        let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
        let previous = handoff::latest_for_repo(&state, repo)
            .ok()
            .flatten()
            .map(|(_, h)| h);
        let (note, source) = handoff::distill_or_structural(
            adapter.as_ref(),
            &distiller_model,
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
            cfg.chrome.events,
            previous.as_ref(),
        );
        let stored = handoff::store(&state, repo, session.as_str(), &note)?;
        // N6: opt-in (`cfg.memory.harvest`, default off) and only from a
        // genuinely distilled handoff -- never the mechanical structural
        // fallback, which has nothing durable to offer. Best-effort: a
        // harvest failure must never turn a successful restart into a
        // failed one.
        if source == "distilled" {
            let _ = super::memory::harvest_durable(
                adapter.as_ref(),
                &distiller_model,
                &note,
                repo,
                &state,
                &mail_slug,
                &cfg,
            );
        }
        announcer.emit(&super::announce::Event::Restart {
            style: source.to_string(),
            stored: stored.display().to_string(),
        });

        restarts += 1;
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "restart",
                detail: &format!("{source} handoff at {}", stored.display()),
                observed_at: None,
            },
        );
        writeln!(
            w,
            "zirv ctx exec: {reason} detected, restarting ({restarts}/{max_restarts}) with a {source} handoff"
        )?;

        // Issue #227: a short backoff before actually relaunching, only for
        // a capacity retry -- a rot/timeout restart is unaffected (the
        // session itself needed a fresh start, not a delay). Reuses the
        // injected `sleep_fn` (the same seam `pace::wait_for_window` already
        // relies on for tests), so no wall-clock time is spent under a fake
        // clock.
        if capacity_exit {
            let backoff = capacity_backoff_secs(restarts);
            if backoff > 0 {
                writeln!(w, "zirv ctx exec: backing off {backoff}s before retrying")?;
                sleep_fn(Duration::from_secs(backoff));
            }
        }

        // Harvest before superseding the transcript so both accounting and
        // any configured whole-run budget include this rot/timeout child.
        harvest_spend(
            adapter.as_ref(),
            &transcript,
            &mut prior_usage,
            &mut prior_tool_calls,
        );
        // Issue #285: reloads and advances the durable objective (if any)
        // against the spend just harvested. Unlike a nudge relaunch, this
        // restart reuses the launch-time `composed` untouched (see this
        // module's own doc comment), so `composed`'s own objective layer --
        // if it has one at all -- is stale; appended beside the handoff
        // below, the one channel that stays live across a rot/timeout/
        // capacity restart.
        let objective_block =
            objective_layer_for_restart(&state, repo, now_fn(), agent::token_spend(&prior_usage));
        session = SessionId::new_v4();
        session_guard.refresh_session(session.as_str());
        // The new session writes somewhere new, so the next iteration's watcher
        // must follow it rather than the file the killed child left behind.
        transcript = derive_transcript(&session);
        transcript_derived = true;
        prompt_args = super::prompt::injection_args_for_session(
            adapter.as_ref(),
            &[],
            composed.as_ref(),
            &state,
            session.as_str(),
        )?;
        // M2: README promises injection attribution "at every session
        // start"; a restart mints a new session id, so it needs its own
        // log entry rather than leaving attribution pinned to the first one.
        super::prompt::log_injection(
            &state,
            "exec",
            session.as_str(),
            composed.as_ref(),
            relaunch_system_prompt_supported,
        );
        announcer.emit(&super::prompt::injection_event(
            composed.as_ref(),
            relaunch_system_prompt_supported,
        ));
        let combined = format!(
            "{prompt_text}\n\n{}",
            handoff::labeled_for_injection(&note, &cfg.screen.thresholds())
        );
        let combined = match &objective_block {
            Some(text) => format!("{combined}{text}"),
            None => combined,
        };
        let combined = super::prompt::task_prompt_with_composed_fallback(
            &combined,
            relaunch_system_prompt_supported,
            composed.as_ref(),
        );
        let mail_in_composed = composed
            .as_ref()
            .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
        // A rot/timeout restart, like a park, does not itself re-list mail,
        // so the fallback for an uninjectable adapter reuses whatever
        // `mail_messages` currently holds -- the launch-time listing, or a
        // nudge's own fresher one if this run was nudged first (Medium 4).
        let combined = super::prompt::task_prompt_with_mail_fallback(
            &combined,
            (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
            parent_short.as_deref(),
        );
        // M8: the user's own extra flags survive the restart too, not just
        // zirv's own (the system prompt args, and now the sandbox/policy
        // prepend) -- this used to be asymmetric.
        let extra: Vec<String> = policy_extra
            .iter()
            .cloned()
            .chain(user_extra.iter().cloned())
            .chain(prompt_args.iter().cloned())
            .collect();
        let (mut rebuilt, sp) = build_headless(&combined, &session, &extra)?;
        rebuilt.current_dir(repo);
        apply_session_env(&mut rebuilt, &session);
        command = rebuilt;
        stdin_prompt = sp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::window::{self, UsageWindows, Window};
    use std::collections::HashMap;

    pub(super) fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Runs the fake agent directly, so `exec` supervises a real child whose
    /// transcript path we control through `--transcript`.
    pub(super) fn fake_agent_command(session: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-agent.sh").display().to_string(),
            "-p".to_string(),
            "do the work".to_string(),
            "--session-id".to_string(),
            session.to_string(),
        ]
    }

    pub(super) fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                format!("sh {}", fixture("fake-agent.sh").display()),
            ),
            // T8: `run_with`'s `sleep_fn` is real `std::thread::sleep` (not
            // test-injectable -- see its own call site), and a fresh temp
            // state dir has no usage source by construction, so every test
            // built on this helper would otherwise pay the real, wall-clock
            // fail-safe delay (default 60s) on every call into `wait_for_
            // window`. Zeroed here, not by lowering the production default:
            // pace.rs's own unit tests already cover the delay's correctness
            // with a `FakeClock`, so exec.rs's tests (which are not testing
            // pacing) should not pay it in real time.
            (
                "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
                "0".to_string(),
            ),
            // Fix round (inline-argv budget regression): these fixtures
            // deliver the composed prompt as one argv element through
            // Git-for-Windows `sh.exe`, which silently truncates a single
            // argument at ~8186 bytes -- the index has its own tests in
            // prompt.rs. A test asserting the index IS present sets this
            // back to "true" explicitly.
            (
                "ZIRV_CTX_PROMPT_SKILL_INDEX".to_string(),
                "false".to_string(),
            ),
        ]
        .into()
    }

    pub(super) fn transcript_for(
        home: &std::path::Path,
        repo: &std::path::Path,
        session: &str,
    ) -> PathBuf {
        home.join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(repo))
            .join(format!("{session}.jsonl"))
    }

    #[test]
    fn a_healthy_run_exits_with_the_childs_own_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    /// T11: the fail-safe blind delay (T8) actually reaches the injected
    /// `sleep_fn` with the right duration -- proof this path is real, not
    /// just claimed by `pace.rs`'s own unit tests, which never touch this
    /// integration seam at all. `base_env` zeros `ZIRV_CTX_PACE_BLIND_DELAY_
    /// SECS` for every other test in this file (see its own doc comment);
    /// this test overrides it back to a small nonzero value specifically so
    /// there is something real to observe, then verifies the observation
    /// through a recording `sleep_fn` rather than actually blocking --
    /// exactly the seam `pace.rs`'s own `FakeClock` tests already use, now
    /// available one layer up.
    #[test]
    fn the_blind_delay_reaches_the_injected_sleep_fn_with_the_right_duration() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "33333333-2222-4333-8444-555555555555";
        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
            "2".to_string(),
        );

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
        assert_eq!(
            slept.borrow().first().copied(),
            Some(2),
            "the blind-mode delay must actually be slept via the injected sleep_fn, got {:?}",
            slept.borrow()
        );
    }

    #[test]
    fn a_failing_child_propagates_its_exit_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3);
    }

    pub(super) fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(home.join(".claude/projects")) else {
            return found;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                if file.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    found.push(file.path());
                }
            }
        }
        found
    }

    pub(super) fn store_provider_collector(
        state_dir: &std::path::Path,
        provider: &str,
        percent: f64,
        limit_reached: bool,
    ) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store_for(
            &state,
            provider,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + 60,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached,
                }),
                seven_day: None,
            },
        )
        .expect("store provider collector state");
    }

    // N4: `zirv ctx nudge` restarting a headless worker.
    //
    // Every test here drives a real `run_with` call whose first agent
    // invocation hangs (`FAKE_AGENT_MODE_FILE` starting with "hang") and is
    // nudged from a background thread once its transcript is up. Like every
    // other test in this module that spawns `sh`/`fake-agent.sh`, these are
    // blocked on Windows by the pre-existing os-193 spawn issue (see this
    // module's other `sh`-spawning tests); written to the same standard the
    // rest of this suite holds regardless.

    /// Polls `path` until it has at least `n` lines or `timeout` elapses,
    /// returning whatever was there either way -- the same "best effort,
    /// bounded wait" shape `run_loop.rs`'s own synchronized tests use via a
    /// marker file, adapted here to a growing log instead of a touch-once
    /// marker since more than one invocation is expected.
    pub(super) fn wait_for_lines(
        path: &std::path::Path,
        n: usize,
        timeout: Duration,
    ) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                if lines.len() >= n {
                    return lines;
                }
            }
            if Instant::now() >= deadline {
                return std::fs::read_to_string(path)
                    .map(|t| t.lines().map(|l| l.to_string()).collect())
                    .unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Nudges whichever session is currently live. `exec` (like `loop`)
    /// keeps exactly one registry record at a time, refreshed on every
    /// restart or park, so resolving the registry is how this test finds the
    /// run it is driving without knowing that session's id up front.
    ///
    /// This used to pass an empty prefix and lean on `starts_with("")`
    /// matching everything. F6 made `zirv ctx nudge` refuse any prefix
    /// shorter than four characters -- a unique-but-mistyped prefix could
    /// otherwise wake, and in `exec`'s case restart, a session the operator
    /// never named -- and an empty prefix is the extreme case of exactly
    /// that. The helper now resolves the live short id and passes it whole,
    /// which is what an operator reading `zirv ctx status` would type.
    pub(super) fn nudge_live_session(
        state_dir: &std::path::Path,
        repo: &std::path::Path,
        message: &str,
    ) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let prefix = crate::commands::ctx::sessions::list(&state)
            .into_iter()
            .find(|(_, liveness)| *liveness == crate::commands::ctx::sessions::Liveness::Live)
            .map(|(record, _)| record.short)
            .expect("exactly one live session to nudge");
        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]
        .into();
        let args = crate::commands::ctx::sessions::NudgeArgs {
            prefix,
            message: Some(message.to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::sessions::run_nudge_with(
            &args,
            &mut out,
            repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge the live session");
    }

    /// `wait_for_lines`, but a give-up (never reaching `n` lines within
    /// `budget`) panics with a clear message instead of silently returning
    /// whatever partial result it has. Every writer-thread test in this
    /// nudge family used to swallow that case (`if lines.is_empty() {
    /// return; }`) and just never nudge -- the only visible symptom, tens of
    /// seconds later, was the launch's own exec timeout (a bare exit 76),
    /// indistinguishable from a real regression. `budget` is sized honestly
    /// against each test's own exec timeout (not the old, uniformly tight
    /// 5s this whole family shared regardless of how much headroom its own
    /// launch actually had), leaving real margin for the rest of the test's
    /// work after the wait. A panic here is caught by the caller's own
    /// `writer.join().expect(...)`, which is what actually fails the test --
    /// this only makes the *reason* legible.
    pub(super) fn wait_for_lines_or_panic(
        path: &std::path::Path,
        n: usize,
        budget: Duration,
    ) -> Vec<String> {
        let lines = wait_for_lines(path, n, budget);
        assert!(
            lines.len() >= n,
            "never saw {n} line(s) in {} within {budget:?} -- the hang-mode agent likely never \
             started, or this machine is starved badly enough that it could not be observed in \
             time (check for CPU contention before assuming a real regression): got {lines:?}",
            path.display()
        );
        lines
    }
}
