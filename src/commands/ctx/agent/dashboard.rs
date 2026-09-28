//! Where one delegation actually runs: joining a live dashboard as a pane,
//! or falling back to an inline supervised run, plus the same-harness and
//! workflow-adoption refusals gating that dispatch.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::CtxResult;
use super::super::adapters;
use super::super::config::{CtxConfig, EnvLookup};
use super::super::dash::spawnreq;
use super::super::envelope;
use super::super::exec;
use super::super::result_schema::Schema;
use super::super::worktree;
use super::*;

/// How long a delegated run waits for the dashboard's own answer before
/// giving up and running headless instead. Generous enough for a live
/// dashboard's own event loop (50ms poll, plus a once-per-tick request
/// sweep) to notice the request and spawn a pane; short enough that an
/// operator who is not actually running a dashboard right now (a stale
/// `DASH_REQUESTS_ENV` inherited from a shell that used to be a pane, whose
/// directory has not yet been reaped) is not kept waiting for long.
///
/// `pub(crate)` (issue #403): `sessions::kill_via_dashboard` waits out the
/// same ceiling for its own request on this same channel, and the two must
/// not drift.
pub(crate) const DASH_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How much longer a *claimed* request is waited out past [`DASH_ACK_TIMEOUT`]
/// before the delegation is called a failure.
///
/// O3: a claim used to be reported as success on the spot -- exit 0, "the
/// dashboard accepted this request" -- for a task that might never run at all
/// (a dashboard that crashed between claiming and spawning leaves exactly that
/// state behind). A claim is good evidence the answer is merely slow, so it
/// buys real extra time; but when the extra time runs out too, the honest
/// answer is a failure, not a success.
pub(super) const DASH_CLAIM_EXTENSION: Duration = Duration::from_secs(10);

/// The stdout line a delegation prints when the dashboard took the request and
/// spawned a *pane* for it. Exit is 0, but nothing has run yet -- a caller that
/// records evidence from a delegated run (the workflow reviewer) has to be able
/// to tell this apart from a completed one, so the prefix is named rather than
/// spelled out at two call sites. The printed text is unchanged.
pub const DASH_SPAWN_ACK_PREFIX: &str = "spawned in dashboard as ";

/// Issue #307.3: every worktree linked to `repo` (`git worktree list
/// --porcelain`), canonicalized, excluding `repo` itself -- best-effort like
/// every other worktree helper in this module (`allocate_worktree`,
/// `reclaim_worktree`): git missing, `repo` not a work tree, or an
/// unparseable/uncanonicalizable path all degrade to an empty list, never a
/// wrong hint. Unlike main's own `adapters::claude::current_worktree_grant_paths`
/// (which this deliberately does not depend on -- that helper is `#[cfg(not
/// (test))]`, so `workdir_visibility_hint`'s own tests could never exercise
/// it), this always runs, the same as every other git shellout in this file.
fn sibling_worktree_paths(repo: &Path) -> Vec<PathBuf> {
    let Ok(canonical_repo) = std::fs::canonicalize(repo) else {
        return Vec::new();
    };
    let Ok(stdout) = worktree::run_git(&canonical_repo, &["worktree", "list", "--porcelain"])
    else {
        return Vec::new();
    };
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter(|path| path != &canonical_repo)
        .collect()
}

/// Issue #307.3: a `--workdir` outside `repo` (and outside every one of
/// `repo`'s own sibling worktrees) is invisible to THIS session's own
/// harness -- the launch-settings work (issue #307) grants the DELEGATED
/// worker prompt-free access to its own `--workdir`, but the delegator
/// itself, if it happens to be a Claude Code session, still cannot read or
/// edit that path without a prompt, because Claude Code's own workspace
/// scope is fixed at session start. `/add-dir <path>` is that harness's own
/// slash command for widening it live, so this nudges toward it rather than
/// silently leaving the operator to discover the prompt themselves.
///
/// Claude-only (`/add-dir` is a Claude Code slash command with no codex
/// equivalent this module knows of): silent unless THIS session's own
/// `AGENT_ENV` reads exactly `"claude"`. Also silent whenever there is
/// nothing to add -- no `--workdir` at all, or one already under `repo` or
/// one of its own worktrees (`adapters::worktree_launch_write_paths`, the
/// same best-effort git discovery `sibling_worktree_paths` above uses --
/// best-effort here too: a detection failure just means no hint, never a
/// wrong one). No new permission rule is added anywhere; this is purely an
/// operator-facing suggestion.
fn workdir_visibility_hint(
    workdir: Option<&Path>,
    repo: &Path,
    env: EnvLookup<'_>,
) -> Option<String> {
    let workdir = workdir?;
    if env(super::super::adapters::AGENT_ENV).as_deref() != Some("claude") {
        return None;
    }
    if workdir.starts_with(repo) {
        return None;
    }
    let already_visible = sibling_worktree_paths(repo)
        .iter()
        .any(|sibling| workdir.starts_with(sibling));
    if already_visible {
        return None;
    }
    Some(format!(
        "hint: run /add-dir {} in this session to read and edit it without prompts",
        workdir.display()
    ))
}

/// Issue #452 (review round 1): the concrete facts a `--json` delegation
/// receipt needs about how a dashboard answered this request -- computed
/// directly from the `SpawnAck`/timeout data `answer_for_ack`/
/// `wait_out_a_claimed_request` already hold, never inferred from the exit
/// code alone. That distinction matters: [`EXIT_DASH_UNCONFIRMED`] and
/// `answer_for_ack`'s own non-retryable, non-budget refusal code are BOTH
/// `1`, but the first means "the dashboard took this request and may still
/// be spawning it" (`launched: true`) and the second means "nothing is
/// running or ever will" (`launched: false`) -- a code-only reading (or a
/// text-parse of the human lines these functions print, an earlier version
/// of this fix did exactly that) cannot tell the two apart.
#[derive(Debug, Clone, Default)]
pub(super) struct AnswerFacts {
    /// Whether the dashboard actually took this request: a pane was
    /// admitted (`ack.ok`), or the request was claimed and not yet
    /// confirmed. `false` only for a definitive refusal.
    pub(super) launched: bool,
    /// The pane's own short session id, when [`Self::launched`] came from a
    /// confirmed spawn.
    pub(super) short: Option<String>,
    pub(super) capability_warnings: Vec<String>,
    /// The claimed-but-unconfirmed notice, or the refusal reason. `None`
    /// only for a confirmed, successful spawn.
    pub(super) reason: Option<String>,
}

/// The requester's own reading of one [`spawnreq::SpawnAck`].
///
/// O2: `ok: false` is two different answers. A policy refusal ends the
/// delegation -- falling back to headless would run a task this operator's own
/// configuration just refused. A `retryable` refusal is the channel saying it
/// could not carry the request, which the headless path was never subject to,
/// so the caller falls through to it with the reason printed. `None` here is
/// exactly that fall-through.
fn answer_for_ack<W: Write>(
    ack: spawnreq::SpawnAck,
    w: &mut W,
    workdir_hint: Option<&str>,
) -> Option<(CtxResult<i32>, AnswerFacts)> {
    if ack.ok {
        let short = ack.short.unwrap_or_default();
        // Issue #230 item 3: the same stdout result surface the headless
        // fork prints to, one line per warning with full detail, so a
        // delegator that joined a live dashboard sees what a headless fork
        // of the same request would have.
        for warning in &ack.capability_warnings {
            if let Err(e) = writeln!(
                w,
                "capability warning: {} -- {}: {}",
                warning.capability, warning.mechanism, warning.detail
            ) {
                return Some((Err(e.into()), AnswerFacts::default()));
            }
        }
        if let Err(e) = writeln!(w, "{DASH_SPAWN_ACK_PREFIX}{short}") {
            return Some((Err(e.into()), AnswerFacts::default()));
        }
        // Issue #307.3: a nudge for THIS session (the delegator), not the
        // spawned worker -- see `workdir_visibility_hint`'s own doc comment.
        if let Some(hint) = workdir_hint
            && let Err(e) = writeln!(w, "{hint}")
        {
            return Some((Err(e.into()), AnswerFacts::default()));
        }
        let facts = AnswerFacts {
            launched: true,
            short: Some(short),
            capability_warnings: capability_warning_lines(&ack.capability_warnings),
            reason: None,
        };
        return Some((Ok(0), facts));
    }
    let reason = ack
        .reason
        .unwrap_or_else(|| "the dashboard refused this request".to_string());
    if ack.retryable {
        eprintln!("zirv ctx agent: {reason}; running headless");
        return None;
    }
    let code = if ack.budget_exhausted {
        exec::EXIT_BUDGET_EXHAUSTED
    } else {
        1
    };
    let facts = AnswerFacts {
        launched: false,
        short: None,
        capability_warnings: Vec::new(),
        reason: Some(reason.clone()),
    };
    Some((
        writeln!(w, "{reason}").map(|_| code).map_err(|e| e.into()),
        facts,
    ))
}

/// O3: a request that was claimed but not acked within [`DASH_ACK_TIMEOUT`]
/// gets [`DASH_CLAIM_EXTENSION`] more, and then an honest answer either way.
///
/// Deliberately **no** headless fallback on the timeout, whatever the outcome:
/// the dashboard holds the claim and may still be spawning the pane, so a
/// second run of the same prompt is the one failure worse than a clear error.
/// A retryable refusal that arrives inside the extension is the one exception,
/// and the ack itself authorises it: the dashboard has answered, and its answer
/// is that it spawned nothing. `None` is that fall-through.
fn wait_out_a_claimed_request<W: Write>(
    dir: &Path,
    stem: &str,
    extension: Duration,
    w: &mut W,
    workdir_hint: Option<&str>,
) -> Option<(CtxResult<i32>, AnswerFacts)> {
    match spawnreq::wait_for_ack(dir, stem, extension) {
        Some(ack) => answer_for_ack(ack, w, workdir_hint),
        None => {
            const NOTICE: &str = "dashboard claimed the request but never confirmed; check zirv \
                                   ctx status / the dashboard";
            let facts = AnswerFacts {
                // The dashboard DID take this request (it is claimed, just
                // not yet confirmed) -- see this function's own doc comment
                // and `AnswerFacts`'s own on why this is `true`, not `false`.
                launched: true,
                short: None,
                capability_warnings: Vec::new(),
                reason: Some(NOTICE.to_string()),
            };
            Some((
                writeln!(w, "{NOTICE}")
                    .map(|_| EXIT_DASH_UNCONFIRMED)
                    .map_err(|e| e.into()),
                facts,
            ))
        }
    }
}

/// The exit code a delegation that could not be confirmed reports. A plain
/// `1`: the task may or may not be running, which for the caller is a failure
/// like any other -- the message on stdout is what says which kind.
pub(super) const EXIT_DASH_UNCONFIRMED: i32 = 1;

/// Where one delegation actually runs, as decided by [`try_join_dashboard`].
#[derive(Debug)]
pub(super) enum Dispatch {
    /// A live dashboard gave a definitive answer -- a pane was spawned, the
    /// request was refused on policy grounds, or it was *claimed* and then
    /// never confirmed even after `DASH_CLAIM_EXTENSION` (O3). The caller
    /// returns the result verbatim and never runs the task itself; the
    /// [`AnswerFacts`] alongside it are what a `--json` delegation builds its
    /// receipt from.
    Answered(CtxResult<i32>, AnswerFacts),
    /// The caller runs the supervised child in this process instead
    /// ([`exec::run_with_report`], unchanged). `no_dashboard` is true only
    /// when no live dashboard could be found anywhere on this machine, which
    /// is the one case that gets [`inline_notice`]'s single line -- every
    /// other fall-through (a prompt that would be misread as a flag, a
    /// request that could not be written, an unclaimed ack timeout, a
    /// `retryable` refusal) has already printed its own reason.
    Inline { no_dashboard: bool },
}

#[cfg(test)]
impl Dispatch {
    fn is_inline(&self) -> bool {
        matches!(self, Dispatch::Inline { .. })
    }

    fn expect_answer(self, message: &str) -> CtxResult<i32> {
        match self {
            Dispatch::Answered(result, _) => result,
            Dispatch::Inline { no_dashboard } => {
                panic!("{message} (fell through inline, no_dashboard={no_dashboard})")
            }
        }
    }
}

/// Asks a live dashboard to spawn `name` as a fresh pane instead of running
/// the supervised child inline in this process: writes a
/// `spawnreq::SpawnRequest` carrying `prompt` as data (never argv, the same
/// discipline every other delegation path in this codebase already holds),
/// then waits up to `DASH_ACK_TIMEOUT` for the matching ack.
///
/// 2026-09-06 (headless removal): "a live dashboard" no longer means "the
/// one this process was spawned inside". `live_join_target` prefers the
/// inherited channel (`spawnreq::DASH_REQUESTS_ENV`), then a live dashboard
/// whose own repo matches this one, then any live dashboard at all -- so a
/// delegation issued from a plain terminal still lands in a visible pane
/// rather than disappearing into a captured subprocess.
///
/// Every ceiling the operator typed travels on the request now
/// (`--max-restarts`/`--timeout-secs`/`--max-tool-calls`, and the trailing
/// `-- <flags>`); the hard error that used to refuse them here, pointing at
/// `--headless`, is gone with the flag itself. What a pane cannot enforce is
/// announced on stderr by [`pane_ceiling_notices`] rather than refused or
/// silently dropped (the F9 rule: never a silent demotion).
// Issue #318 added `result_schema` as this function's 8th parameter, over
// clippy's default 7-argument threshold -- every argument here is already an
// independent, unrelated piece of one delegation's own dashboard-join
// attempt (target, prompt, output, repo, env, two timeouts, and now the
// declared contract), so bundling them into a struct would only move the
// same list one level down without making the one call site clearer.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_join_dashboard<W: Write>(
    args: &AgentArgs,
    prompt: &str,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    ack_timeout: Duration,
    claim_extension: Duration,
    result_schema: Option<&Schema>,
) -> Dispatch {
    // Issue #262: recomputed here rather than threaded in as a parameter --
    // `run_with` already resolved (and, on depth 0, already refused before
    // ever reaching this function) the identical value from the identical
    // `(repo, env)`, so this is one more redundant, deterministic config
    // read, the same trade-off `run_with`'s own doc comment on its `cfg`
    // load already accepts (`exec::run_with` loads its own copy too).
    // `.unwrap_or_else` on a config-load failure falls back to the tightest
    // grant rather than propagating an error out of a function whose return
    // type has no room for one -- `run_with`'s own earlier, identical load
    // already surfaced any real config error before this point.
    let parent_envelope = CtxConfig::load_for_launch(repo, env)
        .ok()
        .map(|cfg| {
            resolve_parent_envelope(&cfg, env)
                .unwrap_or_else(|_| envelope::WorkerEnvelope::locked())
        })
        .unwrap_or_else(envelope::WorkerEnvelope::locked);
    let parent_envelope = &parent_envelope;
    let inherited = env(spawnreq::DASH_REQUESTS_ENV).map(std::path::PathBuf::from);
    let targets = live_join_targets(inherited.as_deref(), env, repo);
    if targets.is_empty() {
        return Dispatch::Inline { no_dashboard: true };
    }
    // A model pin is the one trailing flag a pane can carry across the
    // untrusted request channel -- it travels in `SpawnRequest::model` and
    // the pane re-checks it before building its own argv (`dash::mod::
    // pane_model_args`). Everything else in `flags` is announced as dropped
    // by `pane_ceiling_notices` below rather than refused: an operator who
    // asked for a pane gets one, and finds out on stderr what the pane could
    // not carry.
    //
    // R1-4 (2026-09-06 review): read with `last_model_flag`, not `model_only_
    // flags`. The latter gives up on ANY non-model token, so a caller that
    // pinned a model alongside anything else (`workflow::review::
    // reviewer_args`, whose read-only floor rides in the same list) silently
    // lost the pin and the pane ran on the generic worker default. The value
    // is still re-checked at the fulfilment side (`dash::mod::pane_model_
    // args`: `argv_unsafe_prompt` plus `validate_model_str`); the leading-dash
    // filter here just keeps an obviously bogus pin off the request.
    let pinned_model = super::super::adapters::last_model_flag(&args.flags)
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.starts_with('-'));
    for notice in pane_ceiling_notices(args) {
        eprintln!("zirv ctx agent: {notice}");
    }
    // Defense in depth for the same rule `dash::fulfill_spawn_request`
    // enforces at the authority side: the request's prompt is encoded
    // positionally into the pane's argv, so a prompt shaped like a flag
    // would arrive at the real harness child as one. The inline path this
    // falls back to is safe by construction -- there the prompt travels as
    // the `-p <value>` data it is.
    if super::super::dash::argv_unsafe_prompt(prompt) {
        eprintln!(
            "zirv ctx agent: a prompt beginning with '-' cannot be spawned as a dashboard pane; \
             running inline in this terminal"
        );
        return Dispatch::Inline {
            no_dashboard: false,
        };
    }
    let requested_by = env(super::super::adapters::SESSION_ENV)
        .map(|s| super::super::sessions::short_id(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let req = spawnreq::SpawnRequest {
        kill: None,
        agent: args.name.clone(),
        prompt: prompt.to_string(),
        cwd: repo.to_path_buf(),
        requested_by,
        model: pinned_model.map(str::to_string),
        // This is a scripted request-file hand-off to whatever dashboard
        // picks it up next -- this call site cannot vouch that a human is
        // watching that dashboard, so it does not claim `interactive`.
        interactive: false,
        role: args.role.clone(),
        // Issue #228: a validated, harness-agnostic escape from the
        // dashboard's own repo family -- by the time `run_with` reaches
        // this call site, `args.workdir` (if any) has already passed
        // `validate_workdir`, so this simply carries that canonical path
        // for the fulfilment side to widen into (re-validated there too,
        // since a `SpawnRequest` is untrusted data -- see `validate_
        // workdir`'s own doc comment).
        workdir: args.workdir.clone(),
        // `session_identity`, not `requested_by` above: the two are
        // deliberately separate (see `SpawnRequest::parent_session`'s own
        // doc comment) even though this call site happens to derive both
        // from the same `ZIRV_CTX_SESSION` env var today.
        parent_session: super::super::mail::session_identity(env),
        work_group_id: args.group.clone(),
        budget_tokens: dashboard_budget_tokens(env, args),
        // Issue #155, Phase 6(c): this process's own spawn gate (`run_with`,
        // above) already evaluated `--force` against the reading it had
        // before this request was ever written. Carrying it forward lets
        // `fulfill_spawn_request` honour the identical override rather than
        // re-litigating it blind and refusing a spawn the requester already
        // chose to force -- see `SpawnRequest::force`'s own doc comment.
        force: args.force,
        // Issue #267: carried for parity with the headless path's own
        // `Delegation::mode` and for a future dashboard-side writer-pool
        // enforcement (`SpawnRequest::mode`'s own doc comment) -- a pane
        // spawn does not yet enforce the writer-permit pool itself.
        mode: args.mode,
        owns_workdir: args.worktree,
        // Issue #318: the same canonical JSON `RESULT_SCHEMA_ENV` carries
        // into a headless child's env, so `dash::fulfill_spawn_request` can
        // push it into a fulfilling pane's own child env identically --
        // see that field's own doc comment.
        result_schema: result_schema.map(Schema::to_canonical_json),
        // Issue #262: the parent, not a pre-computed child -- see
        // `SpawnRequest::envelope`'s own doc comment for why the pane
        // fulfilling this request must derive its own child envelope itself.
        envelope: envelope::canonical_json(parent_envelope).ok(),
        path_scope: args.path_scope.clone(),
        no_network: args.no_network,
        depth: args.depth,
        // 2026-09-06: the ceilings a pane spawn used to hard-error on. They
        // only ever NARROW the pane's own supervision, so the fulfilment
        // side honours them even from this untrusted file drop -- see each
        // field's own doc comment for which of the three a pane can
        // actually hold.
        max_restarts: args.max_restarts,
        timeout_secs: args.timeout_secs,
        max_tool_calls: args.max_tool_calls,
        // Carried verbatim, and CLEARED again at the fulfilment side for
        // every file-dropped request (`dash::mod::
        // sanitize_file_dropped_request`) because trailing flags become argv
        // on the pane's real harness child. `pane_ceiling_notices` above
        // already told the operator so.
        flags: args.flags.clone(),
        // R1-4: the seat instructions this delegation asked for, as DATA --
        // the fulfilling pane renders them through its own adapter's
        // injection form, so they survive the sanitiser that clears `flags`
        // and a pane-fulfilled reviewer keeps the seat it was launched for.
        system_prompt: args
            .system_prompt
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        // Issue #543 (review F2): this process's own seat generation, read
        // the same way `seat::guard_from_env`/`seat::env_seat_identity` read
        // theirs -- paired with `parent_session` above so `dash::mod::
        // fulfill_spawn_request` can fence the eventual writer lease against
        // the REQUESTER's identity instead of the dashboard's own env (see
        // `SpawnRequest::parent_seat_generation`'s own doc comment).
        parent_seat_generation: env(super::super::seat::GENERATION_ENV)
            .as_deref()
            .and_then(|g| g.parse::<u64>().ok()),
    };
    // Issue #307.3: computed once, here, and threaded through both this
    // ack and `wait_out_a_claimed_request`'s own -- a nudge for THIS
    // session's own visibility into `--workdir`, not the worker's.
    let workdir_hint = workdir_visibility_hint(args.workdir.as_deref(), repo, env);
    // Issue #620/#627: one attempt PER live candidate. A `retryable` refusal
    // (a foreign-repo dashboard, an unprovable parent claim, a pty that would
    // not open) is the channel declining to carry this request, not a
    // judgement on the task -- so the next live dashboard gets it before the
    // delegation gives up and runs inline. Every other outcome keeps its
    // existing single-attempt semantics.
    let last = targets.len().saturating_sub(1);
    for (index, dir) in targets.iter().enumerate() {
        let dir = dir.as_path();
        let path = match spawnreq::write_request(dir, &req) {
            Ok(path) => path,
            Err(e) => {
                eprintln!(
                    "zirv ctx agent: could not write a spawn request into {}: {e}; running inline \
                     in this terminal",
                    dir.display()
                );
                return Dispatch::Inline {
                    no_dashboard: false,
                };
            }
        };
        let Some(stem) = spawnreq::request_stem(&path) else {
            eprintln!(
                "zirv ctx agent: could not derive a request stem from {}; running inline in this \
                 terminal",
                path.display()
            );
            return Dispatch::Inline {
                no_dashboard: false,
            };
        };
        let inline = Dispatch::Inline {
            no_dashboard: false,
        };
        return match spawnreq::wait_for_ack(dir, &stem, ack_timeout) {
            Some(ack) => match answer_for_ack(ack, w, workdir_hint.as_deref()) {
                Some((result, facts)) => Dispatch::Answered(result, facts),
                None => {
                    if index < last {
                        eprintln!(
                            "zirv ctx agent: trying the next live dashboard ({})",
                            targets[index + 1].display()
                        );
                        continue;
                    }
                    inline
                }
            },
            // F10: `take_requests` takes the request the moment the dashboard
            // picks it up, so a timeout here is ambiguous -- nobody was listening,
            // or somebody took it and is still spawning. Both ends acting on that
            // ambiguity is how one `zirv ctx agent` became two live sessions
            // working the same prompt.
            //
            // F2: the **removal is the decision**, not a check followed by one.
            // This used to ask `is_claimed` and then remove the request, which is
            // check-then-act against a dashboard doing exactly one thing: renaming
            // this very file into its claim (`spawnreq::take_requests`). A claim
            // landing between the check and the remove sent this side headless
            // while the dashboard was already spawning the same prompt. Removing
            // first collapses the two into one atomic operation that only one side
            // can win:
            //
            // * `Ok` -- this process took its own request back off disk before
            //   anybody claimed it, and a dashboard's later rename now finds
            //   nothing, so the headless fallback cannot double-run it;
            // * `Err`, for any reason -- the file is no longer where this process
            //   left it (or cannot be removed), and the thing that moves it is a
            //   claim. Waiting the claim out is the safe reading: the worst case
            //   is an honest "claimed but never confirmed" failure for a request
            //   whose directory vanished with a quitting dashboard, against a
            //   double-run of the operator's task if this guessed the other way.
            None => {
                if std::fs::remove_file(&path).is_ok() {
                    eprintln!(
                        "zirv ctx agent: dashboard did not answer within {ack_timeout:?} (request \
                     was {}); running inline in this terminal",
                        path.display()
                    );
                    return inline;
                }
                match wait_out_a_claimed_request(
                    dir,
                    &stem,
                    claim_extension,
                    w,
                    workdir_hint.as_deref(),
                ) {
                    Some((result, facts)) => Dispatch::Answered(result, facts),
                    None => inline,
                }
            }
        };
    }
    Dispatch::Inline {
        no_dashboard: false,
    }
}

/// The stderr notices a pane spawn owes an operator for everything they
/// typed that a dashboard pane cannot hold exactly the way the inline
/// supervised path would. Never a refusal and never silence -- the F9 rule:
/// a demotion the operator cannot see is worse than either.
///
/// `--max-restarts` and `--timeout-secs` are absent on purpose: a pane's
/// child is never restarted by zirv at all (so any restart budget is already
/// satisfied) and its wall-clock ceiling IS enforced, by `dash::mod::
/// enforce_pane_deadlines`. Only the genuinely unhonoured asks appear.
///
/// R1-7 (2026-09-06 review): `--force` is one of them. A request reaches the
/// dashboard through the file-backed drop directory, and that channel proves
/// nothing about who wrote it -- `dash::mod::intake_channels`' own shared
/// `requests` leaf is writable by any process that discovered this dashboard
/// (issue #179, accepted), so an operator's terminal and a pane's own harness
/// child are indistinguishable there. `sanitize_file_dropped_request`
/// therefore clears `force` for every drop, which means an operator who typed
/// it gets a pane whose spend gate and cross-harness routing were re-decided
/// without it. That is a demotion, so it is announced here rather than
/// silently applied.
fn pane_ceiling_notices(args: &AgentArgs) -> Vec<String> {
    let mut notices = Vec::new();
    if args.max_tool_calls.is_some() {
        notices.push(
            "--max-tool-calls is not enforced on a dashboard pane (no verified tool-call \
             counter); the token budget still is"
                .to_string(),
        );
    }
    if args.force {
        notices.push(
            "--force is not carried to a dashboard pane: a spawn request arrives on a channel \
             that cannot prove an operator wrote it, so the dashboard clears the override and \
             applies its own spend gate and cross-harness routing"
                .to_string(),
        );
    }
    if !args.flags.is_empty() && super::super::adapters::model_only_flags(&args.flags).is_none() {
        notices.push(format!(
            "a dashboard pane carries only a `--model` pin out of `-- {}`; the rest is dropped \
             because a spawn request's trailing flags would become argv on the pane's own harness \
             child",
            args.flags.join(" ")
        ));
    }
    notices
}

/// Rule 3's single line: no live dashboard exists anywhere on this machine,
/// so this delegation runs its supervised child in this terminal. Never a
/// refusal, and this process never launches a dashboard of its own.
pub(super) fn inline_notice(name: &str) -> String {
    format!("zirv ctx agent: no live dashboard -- running {name} inline in this terminal")
}

/// The requests directory `try_join_dashboard` should actually offer the
/// spawn request to, given the one directory it inherited via
/// `DASH_REQUESTS_ENV` (`inherited`).
///
/// The inherited directory is tried first, exactly as before issue #145:
/// absent outright, or present with a dead/missing `owner.pid`, both refuse
/// immediately -- no ack wait is ever spent probing a directory that was
/// never going to answer (issue #144's own fix). Only past that point does
/// issue #145's fallback run: every OTHER `<state>/dash/*` token directory is
/// scanned (`dash::discover_live_dash_dirs`) for a live dashboard to offer
/// the request to instead. The case this recovers: a dashboard restarted (a
/// fresh token dir, a fresh `owner.pid`) while this process's own shell still
/// carries the old, now-dead `DASH_REQUESTS_ENV` value inherited from before
/// the restart -- without this fallback the worker went silently headless
/// even though a perfectly live dashboard was one directory away.
///
/// Selection rule (`dash::select_live_dash_dir`): the live candidate whose
/// `owner.pid` has the newest mtime wins (`owner.pid` is written exactly
/// once, at dashboard startup, so its mtime is that dashboard's own start
/// time -- i.e. the most recently started dashboard wins), tied broken by
/// the lexicographically greatest `<dash_short>-<token>` directory name for a
/// deterministic pick when two dashboards start within the filesystem's own
/// mtime resolution. Deliberately NOT filtered or weighted by the
/// requester's own repo: a candidate whose repo does not match this
/// request's `cwd` is not wasted effort to join -- `fulfill_spawn_request`'s
/// `accepted_spawn_cwd` gate refuses such a request outright with a
/// `retryable` ack, which `answer_for_ack` already reads as "fall back to
/// headless" rather than a hard failure, and any request that gate DOES
/// accept always spawns its pane at the request's own `cwd`, never the
/// dashboard's own -- so joining a dashboard hosting a different repo is
/// display-only (the pane simply appears in that dashboard's own sidebar)
/// and can never misroute the task's working directory. See `dash::
/// discover_live_dash_dirs`'s own doc comment for the full argument.
///
/// Every candidate considered (the inherited directory, and every fallback
/// one) is logged via `eprintln`, live or not and whichever way this
/// resolves -- issue #145's own acceptance criterion is that "my pane never
/// appeared" must be diagnosable from this process's own log alone, with no
/// need to go spelunking through dashboard-side state to find out why.
///
/// `None` only when nothing usable was found anywhere; the caller's existing
/// headless fallback then runs unchanged.
pub(super) fn inherited_dashboard_liveness(
    inherited: &Path,
) -> Option<super::super::sessions::OwnerLiveness> {
    inherited
        .is_dir()
        .then(|| super::super::sessions::dashboard_owner_liveness(inherited))
}

/// The dashboard registry short id encoded in a `<state>/dash/<dash_short>-
/// <token>/requests` path -- the directory name up to its first `-`, which
/// is exactly `spawnreq::request_dir_for`'s own format (a short id is hex,
/// so it never contains one itself).
fn dash_short_of(requests_dir: &Path) -> Option<String> {
    let token_dir = requests_dir.parent()?.file_name()?.to_str()?;
    let (short, _token) = token_dir.split_once('-')?;
    (!short.is_empty()).then(|| short.to_string())
}

/// Pure: whether `candidate` is a live dashboard whose own registry row
/// names `repo`. `dash_shorts_for_repo` is the set of dashboard short ids
/// registered against this repository, resolved once by the caller.
fn candidate_hosts_repo(
    candidate: &super::super::dash::DashCandidate,
    dash_shorts_for_repo: &[String],
) -> bool {
    dash_short_of(&candidate.requests_dir)
        .is_some_and(|short| dash_shorts_for_repo.contains(&short))
}

/// The dashboard sessions currently registered against `repo`, by short id.
/// Best-effort: an unreadable registry simply yields no preference, and the
/// caller falls back to the machine-wide selection rule.
///
/// Issue #620: `Verb::Chat` counts as well as `Verb::Dash`. A dashboard's
/// token directory is named by the dashboard's own short id, which is the
/// short id of the ORCHESTRATOR seat it hosts (`dash::run_dashboard`'s
/// `dashboard_short`) -- and that seat's registry row is `Verb::Chat` from
/// the moment `zirv ctx` restarts it with handoff and it re-registers. Keying
/// this only on `Verb::Dash` therefore lost the live dashboard hosting the
/// caller as soon as its seat was restarted, and a foreign-repo dashboard won
/// the machine-wide rule in its place.
fn dash_shorts_for_repo(state: &super::super::state::StateDir, repo: &Path) -> Vec<String> {
    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    super::super::sessions::list(state)
        .into_iter()
        .filter(|(record, _)| {
            matches!(
                record.verb,
                super::super::sessions::Verb::Dash | super::super::sessions::Verb::Chat
            )
        })
        .filter(|(record, _)| {
            let record_repo =
                std::fs::canonicalize(&record.repo).unwrap_or_else(|_| record.repo.clone());
            record_repo == canonical
        })
        .map(|(record, _)| record.short)
        .collect()
}

/// Which live dashboard this delegation joins: one hosting THIS repository
/// first (its pane lands in the sidebar the operator is already watching for
/// this work), then `dash::select_live_dash_dir`'s machine-wide rule (the
/// most recently started live dashboard). Joining a dashboard that hosts a
/// different repo is display-only and can never misroute the task's working
/// directory -- see `dash::discover_live_dash_dirs`'s own doc comment -- so
/// it stays the fallback rather than a refusal.
#[cfg(test)]
fn select_join_target<'a>(
    state: &super::super::state::StateDir,
    candidates: &'a [super::super::dash::DashCandidate],
    repo: &Path,
    env: EnvLookup<'_>,
) -> Option<&'a super::super::dash::DashCandidate> {
    join_targets_in_order(state, candidates, repo, env)
        .into_iter()
        .next()
}

/// The dashboard process THIS caller is hosted by, from the caller's own
/// registry row: `Record::owner_pid` is the process that filed the record,
/// which for a pane is the dashboard itself (`dash::pane::Pane::spawn` ->
/// `sessions::SessionGuard::register`).
///
/// Issue #620: this is a stronger answer than either selection rule below,
/// because it is not an inference about repositories at all -- it names the
/// dashboard this delegation is literally running inside, whatever verb its
/// row currently carries and whatever repo the registry thinks it holds. A
/// caller with no session identity, no record, or no owner pid simply yields
/// `None` and the ordinary rules decide.
fn hosting_dash_pid(state: &super::super::state::StateDir, env: EnvLookup<'_>) -> Option<u32> {
    let session = env(super::super::adapters::SESSION_ENV)?;
    let short = super::super::sessions::short_id(&session);
    if short.is_empty() {
        return None;
    }
    super::super::sessions::list(state)
        .into_iter()
        .find(|(record, _)| record.short == short)
        .and_then(|(record, _)| record.owner_pid)
}

/// Every live dashboard this delegation may join, best first.
///
/// Order: the dashboard hosting this caller ([`hosting_dash_pid`]), then a
/// live dashboard whose own registry row names THIS repository, then
/// `dash::select_live_dash_dir`'s machine-wide rule (most recently started
/// first) over whatever is left. Joining a dashboard that hosts a different
/// repo is display-only and can never misroute the task's working directory
/// -- see `dash::discover_live_dash_dirs`'s own doc comment -- so it stays a
/// lower-ranked candidate rather than a refusal.
///
/// Issue #620: a LIST rather than one winner, so a foreign-repo dashboard
/// that refuses the request (a `retryable` ack) costs the delegation one
/// round-trip and the next live candidate, instead of ending it inline while
/// a perfectly willing dashboard sits one directory away.
fn join_targets_in_order<'a>(
    state: &super::super::state::StateDir,
    candidates: &'a [super::super::dash::DashCandidate],
    repo: &Path,
    env: EnvLookup<'_>,
) -> Vec<&'a super::super::dash::DashCandidate> {
    let is_live = |c: &super::super::dash::DashCandidate| {
        matches!(c.status, super::super::dash::CandidateStatus::Live { .. })
    };
    let mut ordered: Vec<&'a super::super::dash::DashCandidate> = Vec::new();
    if let Some(owner) = hosting_dash_pid(state, env)
        && let Some(host) = candidates.iter().find(
            |c| matches!(c.status, super::super::dash::CandidateStatus::Live { pid, .. } if pid == owner),
        )
    {
        ordered.push(host);
    }
    let shorts = dash_shorts_for_repo(state, repo);
    if !shorts.is_empty() {
        let own_repo: Vec<super::super::dash::DashCandidate> = candidates
            .iter()
            .filter(|c| candidate_hosts_repo(c, &shorts))
            .cloned()
            .collect();
        if let Some(winner) = super::super::dash::select_live_dash_dir(&own_repo) {
            let chosen = winner.requests_dir.clone();
            if let Some(candidate) = candidates.iter().find(|c| c.requests_dir == chosen)
                && !ordered
                    .iter()
                    .any(|o| o.requests_dir == candidate.requests_dir)
            {
                ordered.push(candidate);
            }
        }
    }
    // Whatever is left, newest-started first -- the same ordering
    // `select_live_dash_dir` applies, just walked rather than maximised.
    let mut rest: Vec<&'a super::super::dash::DashCandidate> = candidates
        .iter()
        .filter(|c| is_live(c))
        .filter(|c| !ordered.iter().any(|o| o.requests_dir == c.requests_dir))
        .collect();
    rest.sort_by(|a, b| match (a.status, b.status) {
        (
            super::super::dash::CandidateStatus::Live { started_at: sa, .. },
            super::super::dash::CandidateStatus::Live { started_at: sb, .. },
        ) => sb
            .cmp(&sa)
            .then_with(|| b.requests_dir.cmp(&a.requests_dir)),
        _ => std::cmp::Ordering::Equal,
    });
    ordered.extend(rest);
    ordered
}

#[cfg(test)]
fn live_join_target(inherited: Option<&Path>, env: EnvLookup<'_>, repo: &Path) -> Option<PathBuf> {
    live_join_targets(inherited, env, repo).into_iter().next()
}

/// [`live_join_target`]'s whole ordered candidate list -- see
/// [`join_targets_in_order`] for why a refusal needs a next one.
fn live_join_targets(inherited: Option<&Path>, env: EnvLookup<'_>, repo: &Path) -> Vec<PathBuf> {
    let inherited_live = matches!(
        inherited.map(|dir| (dir, inherited_dashboard_liveness(dir))),
        Some((_, Some(super::super::sessions::OwnerLiveness::Live)))
    );
    let mut targets = Vec::new();
    if inherited_live && let Some(dir) = inherited {
        targets.push(dir.to_path_buf());
    }
    targets.extend(live_join_fallbacks(inherited, env, repo, inherited_live));
    targets
}

fn live_join_fallbacks(
    inherited: Option<&Path>,
    env: EnvLookup<'_>,
    repo: &Path,
    inherited_live: bool,
) -> Vec<PathBuf> {
    match inherited.map(|dir| (dir, inherited_dashboard_liveness(dir))) {
        // Already the head of the list its caller built: nothing to explain,
        // and the scan below still runs so a refusal has somewhere to go next.
        Some((_, Some(super::super::sessions::OwnerLiveness::Live))) => {}
        Some((dir, Some(super::super::sessions::OwnerLiveness::Dead(pid)))) => {
            eprintln!(
                "zirv ctx agent: {} names a dashboard that already quit (owner.pid names \
                 dead pid {pid}); looking for another live dashboard",
                dir.display()
            );
        }
        Some((dir, Some(super::super::sessions::OwnerLiveness::Missing))) => {
            eprintln!(
                "zirv ctx agent: {} has no readable owner.pid, so no dashboard can be \
                 confirmed live; looking for another live dashboard",
                dir.display()
            );
        }
        Some((dir, None)) => {
            eprintln!(
                "zirv ctx agent: {} (inherited via {}) no longer exists; looking for another live \
                 dashboard",
                dir.display(),
                spawnreq::DASH_REQUESTS_ENV
            );
        }
        // Not inside a dashboard pane at all. Silent: this is the ordinary
        // shape of `zirv agent` from a plain terminal, and the scan below is
        // now its normal path rather than a recovery from something wrong.
        None => {}
    }

    let state = match super::super::state::StateDir::resolve(env) {
        Ok(state) => state,
        Err(e) => {
            eprintln!(
                "zirv ctx agent: could not resolve the state dir to look for a live \
                 dashboard: {e}"
            );
            return Vec::new();
        }
    };
    // The inherited directory may itself live under `state.dash()` and would
    // otherwise show up a second time here, already explained above -- this
    // keeps the fallback log free of a duplicate line for the very directory
    // whose rejection reason was just printed.
    let others: Vec<super::super::dash::DashCandidate> =
        super::super::dash::discover_live_dash_dirs(&state)
            .into_iter()
            .filter(|c| Some(c.requests_dir.as_path()) != inherited)
            .collect();
    // Selected before the log loop below, not after: whether a live candidate
    // is the winner or merely a live-but-passed-over sibling changes what
    // gets printed for it, and every candidate -- winner included -- must
    // appear exactly once. See this function's own doc comment: "every
    // candidate ... is logged, live or not", which a silent `Live => {}` arm
    // here used to violate for every live sibling that lost the selection.
    let ordered = join_targets_in_order(&state, &others, repo, env);
    let winner = ordered.first().copied();
    for candidate in &others {
        let is_winner = winner.is_some_and(|w| w.requests_dir == candidate.requests_dir);
        match candidate.status {
            super::super::dash::CandidateStatus::Live { pid, .. } if !is_winner => eprintln!(
                "zirv ctx agent: candidate {} is live (owner pid {pid}), not selected",
                candidate.requests_dir.display()
            ),
            super::super::dash::CandidateStatus::Live { .. } => {}
            super::super::dash::CandidateStatus::NoOwnerPid => eprintln!(
                "zirv ctx agent: candidate {} has no owner.pid; skipped",
                candidate.requests_dir.display()
            ),
            super::super::dash::CandidateStatus::DeadOwner(pid) => eprintln!(
                "zirv ctx agent: candidate {} names dead pid {pid}; skipped",
                candidate.requests_dir.display()
            ),
        }
    }
    match winner {
        Some(winner) => {
            // `instead` only when something was actually rejected above: a
            // live inherited channel is the head of the list, not a
            // replacement for one.
            if inherited_live {
                eprintln!(
                    "zirv ctx agent: {} is also live and available if the first refuses",
                    winner.requests_dir.display()
                );
            } else {
                eprintln!(
                    "zirv ctx agent: joining {} instead",
                    winner.requests_dir.display()
                );
            }
        }
        None => {
            if !inherited_live {
                let considered: Vec<String> = others
                    .iter()
                    .map(|c| c.requests_dir.display().to_string())
                    .collect();
                eprintln!(
                    "zirv ctx agent: no live dashboard found under {} ({})",
                    state.dash().display(),
                    if considered.is_empty() {
                        "no other candidates".to_string()
                    } else {
                        format!("candidates: {}", considered.join(", "))
                    }
                );
            }
        }
    }
    ordered
        .into_iter()
        .map(|candidate| candidate.requests_dir.clone())
        .collect()
}

/// Issue #223 §E: `workflow.adoption = enforce`'s delegation gate. Refuses
/// only when all four hold -- the policy is `enforce`, this call carries a
/// session identity ([`adapters::SESSION_ENV`]), that session's own adoption
/// record (`hook::adoption_record_path`/`load_adoption_record`, the same
/// record `ctx::hook`'s Stop/Prompt handlers maintain) says substantial, and
/// no `zirv workflow` is active for `repo` right now -- so an operator
/// running `zirv agent` from a plain shell with no session identity at all is
/// never blocked.
pub(super) fn adoption_enforcement_refusal(
    state: &super::super::state::StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
) -> Option<String> {
    use crate::commands::workflow::adoption::AdoptionPolicy;

    if cfg.workflow.adoption != AdoptionPolicy::Enforce {
        return None;
    }
    let session = env(adapters::SESSION_ENV)?;
    let path = super::super::hook::adoption_record_path(state, &session);
    let record = super::super::hook::load_adoption_record(&path);
    if !record.substantial {
        return None;
    }
    if crate::commands::workflow::engine::load_active(state, repo)
        .ok()
        .flatten()
        .is_some()
    {
        return None;
    }
    Some(format!(
        "zirv agent: held by workflow.adoption = enforce -- this session has done substantial \
         work ({} edit calls over {} turns) with no active zirv workflow. Start one first: zirv \
         workflow start --task \"<summary>\"",
        record.edit_like_calls, record.turns
    ))
}

/// Issue #328: from an orchestrator seat, `zirv agent <own harness>` is
/// refused unless it is a work-group operation or `--force`. Same-harness
/// delegation belongs to the harness's own native subagent tool -- it stays
/// visible in the calling session and returns its result directly, which a
/// zirv-supervised headless worker cannot do; `zirv agent` exists for
/// another harness, or for a work group/sub-orchestrator (a distinct
/// concept from "delegate to the same harness"), not as a second way to
/// spawn a subagent on the seat's own harness.
///
/// Refuses only when ALL of: the seat is an orchestrator ([`adapters::
/// SEAT_ROLE_ENV`]); this session's own harness ([`adapters::AGENT_ENV`])
/// trimmed equals `args.name` trimmed, case-insensitively; `args.role` is
/// not `"sub-orchestrator"`; `args.group` is unset; and `args.force` is
/// false. Any one of those failing is enough to let the delegation through
/// -- an unknown own-harness (`AGENT_ENV` unset) never refuses either,
/// since there is nothing to compare `args.name` against.
pub(super) fn same_harness_refusal(args: &AgentArgs, env: EnvLookup<'_>) -> Option<String> {
    if env(adapters::SEAT_ROLE_ENV).as_deref() != Some("orchestrator") {
        return None;
    }
    let own_harness = env(adapters::AGENT_ENV)?;
    if !own_harness.trim().eq_ignore_ascii_case(args.name.trim()) {
        return None;
    }
    if args.role.as_deref() == Some("sub-orchestrator") || args.group.is_some() || args.force {
        return None;
    }
    let other = adapters::ADAPTERS
        .iter()
        .map(|(adapter_name, _)| *adapter_name)
        .find(|adapter_name| !adapter_name.eq_ignore_ascii_case(args.name.trim()))
        .unwrap_or("<other-harness>");
    Some(format!(
        "zirv ctx agent: '{name}' is this session's own harness. From an orchestrator seat, \
         same-harness delegation uses the harness's native subagent tool (it stays visible in \
         this session and returns its result directly); `zirv agent` is for another harness \
         (e.g. `zirv agent {other}`) or a work group (`--role sub-orchestrator --scope \
         \"<area>\"`). Pass --force to spawn a zirv-supervised worker anyway.",
        name = args.name,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::hook;
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::window;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::super::tests::*;

    /// Issue #223 §E: `workflow.adoption = enforce` refuses a delegation when
    /// all four conditions hold -- enforce policy, a session identity, a
    /// substantial adoption record, and no active workflow.
    #[test]
    fn enforce_refuses_a_substantial_session_with_no_active_workflow() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(root.path().to_path_buf());
        let session = "sess-enforce-1";
        let path = hook::adoption_record_path(&state, session);
        hook::save_adoption_record(&path, &hook::AdoptionRecord::substantial_for_test(7, 9));

        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = crate::commands::workflow::adoption::AdoptionPolicy::Enforce;
        let env: HashMap<String, String> =
            [(adapters::SESSION_ENV.to_string(), session.to_string())].into();

        let message =
            adoption_enforcement_refusal(&state, repo.path(), &cfg, &|k| env.get(k).cloned())
                .expect("must refuse");
        assert!(message.contains("workflow.adoption = enforce"), "{message}");
        assert!(message.contains("7 edit calls over 9 turns"), "{message}");
        assert!(
            message.contains("zirv workflow start"),
            "must point at the fix: {message}"
        );
    }

    /// An active workflow lifts the gate even though the record still says
    /// substantial.
    #[test]
    fn enforce_allows_a_substantial_session_with_an_active_workflow() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(root.path().to_path_buf());
        let session = "sess-enforce-2";
        let path = hook::adoption_record_path(&state, session);
        hook::save_adoption_record(&path, &hook::AdoptionRecord::substantial_for_test(7, 9));
        crate::commands::workflow::engine::save(
            &state,
            &crate::commands::workflow::engine::WorkflowState::start(
                repo.path().to_path_buf(),
                "small feature".into(),
                crate::commands::workflow::engine::WorkflowKind::Feature,
                None,
                true,
                test_classification(),
            ),
            true,
        )
        .expect("save active workflow");

        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = crate::commands::workflow::adoption::AdoptionPolicy::Enforce;
        let env: HashMap<String, String> =
            [(adapters::SESSION_ENV.to_string(), session.to_string())].into();

        assert_eq!(
            adoption_enforcement_refusal(&state, repo.path(), &cfg, &|k| env.get(k).cloned()),
            None,
            "an active workflow must lift the gate"
        );
    }

    /// `nudge` (or any policy below `enforce`) never gates a delegation.
    #[test]
    fn enforce_gate_is_inert_below_the_enforce_policy() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(root.path().to_path_buf());
        let session = "sess-enforce-3";
        let path = hook::adoption_record_path(&state, session);
        hook::save_adoption_record(&path, &hook::AdoptionRecord::substantial_for_test(7, 9));

        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = crate::commands::workflow::adoption::AdoptionPolicy::Nudge;
        let env: HashMap<String, String> =
            [(adapters::SESSION_ENV.to_string(), session.to_string())].into();

        assert_eq!(
            adoption_enforcement_refusal(&state, repo.path(), &cfg, &|k| env.get(k).cloned()),
            None,
            "nudge must never block a delegation"
        );
    }

    /// An operator with no session identity at all (a plain shell, not a
    /// supervised harness session) is never blocked.
    #[test]
    fn enforce_gate_never_blocks_an_operator_with_no_session_identity() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(root.path().to_path_buf());
        // No adoption record is even written: there is no session id to key
        // one on.
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = crate::commands::workflow::adoption::AdoptionPolicy::Enforce;
        let empty: HashMap<String, String> = HashMap::new();

        assert_eq!(
            adoption_enforcement_refusal(&state, repo.path(), &cfg, &|k| empty.get(k).cloned()),
            None,
            "no session identity means no session to gate"
        );
    }

    // -- same_harness_refusal (issue #328) ---------------------------------

    /// The baseline refusal shape: an orchestrator seat asking `zirv agent`
    /// to delegate to its own harness.
    #[test]
    fn same_harness_refusal_refuses_the_orchestrators_own_harness() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let args = args_for("claude", "go");
        let message = same_harness_refusal(&args, &|k| env.get(k).cloned())
            .expect("same-harness delegation from an orchestrator seat is refused");
        assert!(message.contains("own harness"), "got {message}");
        assert!(message.contains("--force"), "got {message}");
    }

    #[test]
    fn same_harness_refusal_allows_force() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let mut args = args_for("claude", "go");
        args.force = true;
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_none());
    }

    #[test]
    fn same_harness_refusal_allows_a_sub_orchestrator_role() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let mut args = args_for("claude", "go");
        args.role = Some("sub-orchestrator".to_string());
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_none());
    }

    #[test]
    fn same_harness_refusal_allows_a_work_group() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let mut args = args_for("claude", "go");
        args.group = Some("g1".to_string());
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_none());
    }

    #[test]
    fn same_harness_refusal_allows_a_different_harness() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let args = args_for("codex", "go");
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_none());
    }

    #[test]
    fn same_harness_refusal_allows_with_no_seat_role_env() {
        let env = env_map(&[(adapters::AGENT_ENV, "claude")]);
        let args = args_for("claude", "go");
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_none());
    }

    #[test]
    fn same_harness_refusal_allows_a_sub_orchestrator_seat() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "sub-orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let args = args_for("claude", "go");
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_none());
    }

    /// The own-harness comparison is case-insensitive: `Claude` still
    /// matches an `AGENT_ENV` of `claude`.
    #[test]
    fn same_harness_refusal_matches_the_own_harness_case_insensitively() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let args = args_for("Claude", "go");
        assert!(same_harness_refusal(&args, &|k| env.get(k).cloned()).is_some());
    }

    // Task 11: joining a running dashboard instead of spawning headless.

    /// Polls `dir` for a `req-*.json` file and hands its file stem to
    /// `respond` (which writes whatever answer the test wants), returning the
    /// request's own raw contents -- the same "responder" shape every
    /// dashboard-join test below needs, since `write_request` mints a random
    /// uuid filename the test cannot know in advance. Never touches a real
    /// agent: this only ever races against `try_join_dashboard`'s own polling
    /// loop, both confined to a tempdir.
    fn intercept_next_request(
        dir: std::path::PathBuf,
        respond: impl Fn(&std::path::Path, &str),
    ) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                    if name.starts_with("req-") && name.ends_with(".json") {
                        let contents = std::fs::read_to_string(&path).expect("read request");
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .expect("file stem")
                            .to_string();
                        respond(&dir, &stem);
                        return contents;
                    }
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("no spawn request appeared within the deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn respond_to_next_request(dir: std::path::PathBuf, ack_body: &'static str) -> String {
        intercept_next_request(dir, move |dir, stem| {
            std::fs::write(dir.join(format!("ack-{stem}.json")), ack_body).expect("write ack");
        })
    }

    fn respond_if_request(dir: PathBuf, timeout: Duration) -> Option<String> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() <= deadline {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = path.file_name().and_then(|name| name.to_str())?;
                    if name.starts_with("req-") && name.ends_with(".json") {
                        let contents = std::fs::read_to_string(&path).expect("read request");
                        let stem = path.file_stem().and_then(|stem| stem.to_str())?;
                        std::fs::write(
                            dir.join(format!("ack-{stem}.json")),
                            r#"{"ok":true,"short":"abcd1234","reason":null}"#,
                        )
                        .expect("write ack");
                        return Some(contents);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// A live dashboard (env set, directory present) that acks `ok: true`
    /// short-circuits the headless path entirely: `run_with` reports the
    /// pane's own short id and returns `Ok(0)`, and the request it wrote
    /// carries the prompt as data, not argv.
    #[test]
    fn dashboard_join_spawns_a_pane_and_reports_its_short_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let args = joinable_args("claude", "a specific delegated task");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as abcd1234"),
            "got {output}"
        );
        assert!(
            request_body.contains("a specific delegated task"),
            "the prompt must travel as data in the request file: {request_body}"
        );
    }

    #[test]
    fn mcp_workspace_stays_on_the_validated_inline_adapter_after_a_limit() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(home.join(".codex")).expect("codex config dir");
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("zirv config dir");
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'docs'\n",
        )
        .expect("codex config");
        std::fs::write(
            tmp.path().join(".zirv/ctx.toml"),
            "[[workspace]]\nname = 'docs'\nmcp_servers = ['docs']\n",
        )
        .expect("workspace config");

        let state = StateDir::from_root(state_dir.clone());
        let now = crate::commands::ctx::state::now_secs();
        window::store_for(
            &state,
            window::CODEX_USAGE_PROVIDER,
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: 100.0,
                    resets_at: now + 60,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: true,
                }),
                seven_day: None,
            },
        )
        .expect("store usage");

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("modes");
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
        let (requests_dir, mut env) = live_dashboard_dir(tmp.path());
        env.insert("HOME".into(), home.display().to_string());
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".into(), "0".into());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".into(), "0".into());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".into(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );
        let responder =
            std::thread::spawn(move || respond_if_request(requests_dir, Duration::from_secs(3)));

        let mut args = joinable_args("codex", "use the docs server");
        args.workspace = Some("docs".into());
        args.force = true;
        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("workspace worker runs");
        let dashboard_request = responder.join().expect("responder");

        assert_eq!(code, 0);
        assert!(
            dashboard_request.is_none(),
            "an MCP-bound worker must not be handed to a dashboard that can reroute it"
        );
        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "{log}");
        assert!(!log.contains("\"action\":\"harness-handover\""), "{log}");
    }

    /// A lone `--model` pin is the one trailing flag a pane can honour, so it
    /// joins the dashboard instead of declining to the headless path -- the
    /// harness layer now teaches orchestrators to write one on every
    /// delegation, and declining would cost a dashboard session its pane every
    /// time. The pinned model travels in the request, for the fulfilment side
    /// to build into the pane's own argv.
    #[test]
    fn a_lone_model_pin_still_joins_the_dashboard_and_travels_in_the_request() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "go");
        args.flags = vec!["--model".to_string(), "haiku".to_string()];
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(
            req.model.as_deref(),
            Some("haiku"),
            "the pinned model reaches the fulfilment side: {request_body}"
        );
    }

    /// R1-4: a reviewer-shaped delegation -- seat instructions plus a model
    /// pin plus the adapter's own read-only floor -- used to reach a pane
    /// with NEITHER. `model_only_flags` gives up on any non-model token, so
    /// `SpawnRequest::model` was `None` and the pane ran the generic worker
    /// default; the seat instructions rode `flags`, which the fulfilment side
    /// clears outright. Both now travel as request data.
    #[test]
    fn a_reviewer_shaped_delegation_carries_its_model_and_seat_prompt_to_the_pane() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "review this package");
        args.system_prompt =
            Some("zirv workflow agent seat: reviewer@1\nrole: reviewer".to_string());
        // Exactly what `workflow::review::reviewer_args` puts in `flags`:
        // the model pin never travels alone.
        args.flags = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string(),
        ];
        assert!(
            super::super::adapters::model_only_flags(&args.flags).is_none(),
            "sanity: the old extraction gives up on this exact shape"
        );

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(
            req.model.as_deref(),
            Some("opus"),
            "the pinned review model must reach the pane: {request_body}"
        );
        assert!(
            req.system_prompt
                .as_deref()
                .is_some_and(|text| text.contains("workflow agent seat: reviewer@1")),
            "and so must the seat instructions: {request_body}"
        );
    }

    /// The other fork of the same delegation: an inline supervised child must
    /// hear the identical seat instructions, through the adapter's own
    /// injection form and ahead of the operator's own trailing flags (so a
    /// model pin still wins under CLI last-occurrence semantics).
    #[test]
    fn a_seat_prompt_reaches_an_inline_child_through_the_adapters_own_injection_form() {
        let adapter = super::super::super::adapters::claude::ClaudeAdapter::new(None);
        let mut args = args_for("claude", "review this package");
        args.flags = vec!["--model".to_string(), "opus".to_string()];

        assert_eq!(
            flags_with_system_prompt(&args, &adapter),
            args.flags,
            "no seat instructions changes nothing at all"
        );

        args.system_prompt = Some("zirv workflow agent seat: reviewer@1".to_string());
        assert_eq!(
            flags_with_system_prompt(&args, &adapter),
            vec![
                "--append-system-prompt".to_string(),
                "zirv workflow agent seat: reviewer@1".to_string(),
                "--model".to_string(),
                "opus".to_string(),
            ],
            "the seat text is rendered by the adapter and lands before the operator's own flags"
        );

        args.system_prompt = Some("   ".to_string());
        assert_eq!(
            flags_with_system_prompt(&args, &adapter),
            args.flags,
            "a blank seat prompt is nothing to inject"
        );
    }

    /// Issue #155, Phase 5(c): `--role`/`--group` travel on the request for
    /// the fulfilment side's depth cap and budget resolution.
    #[test]
    fn role_and_group_join_the_dashboard_and_travel_in_the_request() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, mut env) = live_dashboard_dir(tmp.path());
        // This request's own lineage: the calling process's own session id,
        // which `parent_session` is derived from (`mail::session_identity`).
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "eeee5555".to_string(),
        );

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "go");
        args.role = Some("sub-orchestrator".to_string());
        args.group = Some("wg-1".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0, "must NOT decline for --role/--group");
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(req.role.as_deref(), Some("sub-orchestrator"));
        assert_eq!(req.work_group_id.as_deref(), Some("wg-1"));
        assert!(
            req.parent_session.is_some(),
            "this process's own session id must be the lineage link: {request_body}"
        );
    }

    #[test]
    fn dashboard_group_request_carries_only_the_remaining_token_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let group = crate::commands::ctx::group::WorkGroup {
            work_group_id: "wg-remaining".to_string(),
            parent_session_id: String::new(),
            scope: "bounded dashboard work".to_string(),
            child_limit: 3,
            token_budget: Some(400_000),
            spent_tokens: 125_000,
            reserved_tokens: 0,
            deadline_secs: None,
            completion_contract: String::new(),
            created_at: crate::commands::ctx::state::now_secs(),
            closed_at: None,
            admitted_children: 0,
            sub_orchestrator_session: None,
        };
        crate::commands::ctx::group::create(&state, &group).expect("create group");

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "bounded work");
        args.group = Some("wg-remaining".to_string());
        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(
            req.budget_tokens,
            Some(275_000),
            "the pane request must carry the group's remaining ceiling"
        );
    }

    /// A refusal ack (the dashboard's own `cfg.agents.refusal` gate, or an
    /// unknown/unready adapter) prints the reason and returns `Ok(1)` --
    /// still short-circuiting the headless path, since the dashboard already
    /// gave a definitive answer.
    #[test]
    fn dashboard_join_prints_the_refusal_reason_and_returns_exit_1() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"claude is disabled by .zirv/.settings.toml"}"#,
                )
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        responder.join().expect("responder thread");

        assert_eq!(code, 1);
        let output = String::from_utf8_lossy(&out);
        assert!(output.contains("disabled"), "got {output}");
    }

    #[test]
    fn dashboard_budget_refusal_returns_the_budget_exhausted_exit() {
        let mut out = Vec::new();
        let (result, facts) = answer_for_ack(
            spawnreq::SpawnAck {
                ok: false,
                short: None,
                reason: Some("budget-exhausted: work group budget is spent".to_string()),
                retryable: false,
                budget_exhausted: true,
                capability_warnings: Vec::new(),
            },
            &mut out,
            None,
        )
        .expect("a policy refusal is final");
        let result = result.expect("writes the result");

        assert_eq!(result, exec::EXIT_BUDGET_EXHAUSTED);
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("budget-exhausted")
        );
        // Issue #452 (review round 1): a refusal never launched anything --
        // this is exactly the fact `dashboard_answer_receipt` needs to avoid
        // reporting `launched` for a plain policy refusal.
        assert!(!facts.launched);
        assert_eq!(
            facts.reason.as_deref(),
            Some("budget-exhausted: work group budget is spent")
        );
    }

    /// Issue #452 (review round 1): `dashboard_answer_receipt` reads `state`
    /// off `AnswerFacts::launched`, never off the exit code -- a refusal
    /// reports `launch_failed` and carries the refusal `reason`, regardless
    /// of which non-zero code it exited with.
    #[test]
    fn dashboard_answer_receipt_reports_launch_failed_for_unlaunched_facts() {
        let facts = AnswerFacts {
            launched: false,
            short: None,
            capability_warnings: Vec::new(),
            reason: Some("claude is disabled by .zirv/.settings.toml".to_string()),
        };
        let receipt = dashboard_answer_receipt(&args_for("claude", "go"), None, 1, &facts);
        assert_eq!(receipt.mode, DelegationMode::DashboardPane);
        assert_eq!(receipt.state, DelegationState::LaunchFailed);
        assert_eq!(receipt.exit_code, Some(1));
        assert_eq!(receipt.session, None);
        assert_eq!(
            receipt.reason.as_deref(),
            Some("claude is disabled by .zirv/.settings.toml")
        );
    }

    /// Issue #452 (review round 1): a `--json` delegation refused before
    /// `try_join_dashboard` even runs (delegation depth 0, here) must still
    /// print exactly one JSON receipt -- and nothing else -- to stdout, with
    /// `state: launch_failed`. This is the cheapest such path: no process
    /// spawn, no dashboard, no `--task` card to seed.
    #[test]
    fn a_json_delegation_refused_before_dispatch_prints_only_one_launch_failed_receipt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");

        let depth_zero = envelope::WorkerEnvelope {
            delegation_depth: 0,
            ..root_envelope(&CtxConfig::default())
        };
        let mut env = base_env(&state_path);
        env.insert(
            ENVELOPE_ENV.to_string(),
            envelope::canonical_json(&depth_zero).expect("serialize envelope"),
        );

        let args = AgentArgs {
            json: true,
            ..args_for("claude", "go")
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a depth-0 refusal returns Ok(2), never a hard Err");
        assert_eq!(code, 2);

        let stdout = String::from_utf8(out).expect("utf8");
        // The whole point of `--json` is that stdout is ONE parseable JSON
        // value and nothing else -- `serde_json::from_str` on a `Value`
        // refuses trailing non-whitespace content, so a stray human line
        // ahead of or after the receipt fails this parse, not just a
        // `.contains` check.
        let receipt: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("stdout must be exactly one JSON value: {e}\n{stdout}"));
        assert_eq!(receipt["state"], "launch_failed");
        assert_eq!(receipt["mode"], "inline");
        assert_eq!(receipt["exit_code"], 2);
        assert!(
            receipt["reason"]
                .as_str()
                .expect("reason present")
                .contains("depth 0"),
            "got {receipt}"
        );
    }

    // -- workdir_visibility_hint (issue #307.3) ----------------------------

    #[test]
    fn workdir_visibility_hint_is_none_without_a_workdir() {
        let repo = crate::commands::ctx::testenv::repo();
        let env = env_map(&[(super::super::super::adapters::AGENT_ENV, "claude")]);
        assert_eq!(
            workdir_visibility_hint(None, repo.path(), &|k| env.get(k).cloned()),
            None
        );
    }

    #[test]
    fn workdir_visibility_hint_is_none_for_a_non_claude_session() {
        let repo = crate::commands::ctx::testenv::repo();
        let outside = tempfile::tempdir().expect("tempdir");
        for agent in [None, Some("codex")] {
            let env = match agent {
                Some(agent) => env_map(&[(super::super::super::adapters::AGENT_ENV, agent)]),
                None => env_map(&[]),
            };
            assert_eq!(
                workdir_visibility_hint(Some(outside.path()), repo.path(), &|k| env
                    .get(k)
                    .cloned()),
                None,
                "agent={agent:?}"
            );
        }
    }

    #[test]
    fn workdir_visibility_hint_is_none_when_the_workdir_is_inside_repo() {
        let repo = crate::commands::ctx::testenv::repo();
        let inside = repo.path().join("sub");
        std::fs::create_dir_all(&inside).expect("mkdir");
        let env = env_map(&[(super::super::super::adapters::AGENT_ENV, "claude")]);
        assert_eq!(
            workdir_visibility_hint(Some(&inside), repo.path(), &|k| env.get(k).cloned()),
            None
        );
    }

    /// The one case the hint is meant for: a Claude session delegating to a
    /// `--workdir` genuinely outside its own repo.
    #[test]
    fn workdir_visibility_hint_names_the_path_for_a_claude_session_with_an_external_workdir() {
        let repo = crate::commands::ctx::testenv::repo();
        let outside = tempfile::tempdir().expect("tempdir");
        let env = env_map(&[(super::super::super::adapters::AGENT_ENV, "claude")]);
        let hint =
            workdir_visibility_hint(Some(outside.path()), repo.path(), &|k| env.get(k).cloned())
                .expect("a claude session with an external workdir gets a hint");
        assert!(hint.starts_with("hint: run /add-dir "), "got {hint}");
        assert!(
            hint.contains(&outside.path().display().to_string()),
            "got {hint}"
        );
        assert!(hint.contains("without prompts"), "got {hint}");
    }

    /// End-to-end: the hint reaches stdout on the exact line after the
    /// dashboard's own spawn ack, and only for a claude session.
    #[test]
    fn dashboard_join_appends_the_workdir_visibility_hint_for_a_claude_session() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let outside = tempfile::tempdir().expect("tempdir");
        assert!(git_init(outside.path()), "git init");
        let (requests_dir, mut env) = live_dashboard_dir(tmp.path());
        env.insert(
            super::super::super::adapters::AGENT_ENV.to_string(),
            "claude".to_string(),
        );

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"cafe0001","reason":null}"#)
        });

        let mut args = joinable_args("claude", "a task outside the repo");
        args.workdir = Some(outside.path().to_path_buf());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as cafe0001"),
            "got {output}"
        );
        // `--workdir` reaches this point through `validate_workdir`'s own
        // canonicalization (`run_with`'s `canonical_workdir`), which on
        // Windows can add a `\\?\` verbatim prefix -- compare against that
        // SAME canonical form rather than the raw tempdir path.
        let canonical = std::fs::canonicalize(outside.path()).expect("canonicalize");
        assert!(
            output.contains(&format!("hint: run /add-dir {}", canonical.display())),
            "got {output}"
        );
    }

    /// Security review round 2 (Finding 4): `--scope` mints a group, and a
    /// dashboard that then refuses the spawn means the delegation never
    /// starts -- so the group must not be left open, unclaimed and childless
    /// on disk. (The refusal here is the same non-retryable shape the
    /// dashboard's own lineage and depth gates produce.)
    #[test]
    fn a_refused_scope_spawn_leaves_no_work_group_behind() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"a worker may not delegate onward"}"#,
                )
            }
        });

        let mut args = joinable_args("claude", "own this scope");
        args.role = Some("sub-orchestrator".to_string());
        args.scope = Some("the frontend rewrite".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 1, "a policy refusal ends the delegation");
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert!(
            req.work_group_id.is_some(),
            "the minted group still travels in the request: {request_body}"
        );
        assert!(
            crate::commands::ctx::group::list(&state).is_empty(),
            "a refused spawn must leave no group behind: {:?}",
            crate::commands::ctx::group::list(&state)
        );
    }

    /// A live dashboard this process was never told about: a token directory
    /// under `<state>/dash` with a live `owner.pid`, and deliberately NO
    /// `DASH_REQUESTS_ENV` in the environment at all.
    fn uninherited_live_dashboard(root: &Path) -> (PathBuf, HashMap<String, String>) {
        let state_dir = root.join("state");
        let requests_dir = state_dir
            .join("dash")
            .join("aaaa1111-0123456789abcdef")
            .join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");
        std::fs::write(
            requests_dir.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");
        (requests_dir, base_env(&state_dir))
    }

    /// Rule 2: a live dashboard is joined even when this process was never
    /// spawned inside one -- no `DASH_REQUESTS_ENV`, just a live token dir
    /// under `<state>/dash`. The request lands there and the ack path answers
    /// exactly as it does for an inherited channel.
    #[test]
    fn a_live_dashboard_this_process_never_inherited_is_still_joined() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = uninherited_live_dashboard(tmp.path());
        assert!(
            !env.contains_key(crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV),
            "this process inherits no dashboard channel"
        );

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let args = joinable_args("claude", "a task from a plain terminal");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("the uninherited dashboard join runs");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        assert!(
            String::from_utf8_lossy(&out).contains("spawned in dashboard as abcd1234"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            request_body.contains("a task from a plain terminal"),
            "the prompt travels as data: {request_body}"
        );
    }

    /// Rule 3: no live dashboard anywhere means the supervised child runs in
    /// this terminal, announced by exactly one line. Never a refusal, and
    /// this process never launches a dashboard of its own.
    #[test]
    fn no_live_dashboard_anywhere_runs_inline_behind_one_notice_line() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let dispatch = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(50),
            Duration::from_millis(50),
            None,
        );

        assert!(
            matches!(dispatch, Dispatch::Inline { no_dashboard: true }),
            "got {dispatch:?}"
        );
        assert!(out.is_empty(), "nothing is reported as spawned");

        let notice = inline_notice(&args.name);
        assert_eq!(notice.lines().count(), 1, "exactly one line: {notice}");
        assert!(notice.contains("no live dashboard"), "got {notice}");
        assert!(notice.contains("claude"), "must name the agent: {notice}");
    }

    /// Rule 2's preference order: a dashboard whose own registry row names
    /// THIS repository wins over any other live one. These two pure helpers
    /// are what turns a `<state>/dash/<short>-<token>/requests` path back
    /// into the dashboard short id the registry is keyed by.
    #[test]
    fn a_dash_candidates_repo_is_matched_through_its_short_id() {
        let dir = Path::new("/state/dash/aaaa1111-0123456789abcdef/requests");
        assert_eq!(dash_short_of(dir).as_deref(), Some("aaaa1111"));
        assert_eq!(dash_short_of(Path::new("requests")), None);

        let candidate = super::super::super::dash::DashCandidate {
            requests_dir: dir.to_path_buf(),
            status: super::super::super::dash::CandidateStatus::NoOwnerPid,
        };
        assert!(candidate_hosts_repo(
            &candidate,
            &["aaaa1111".to_string(), "bbbb2222".to_string()]
        ));
        assert!(
            !candidate_hosts_repo(&candidate, &["bbbb2222".to_string()]),
            "a dashboard registered against another repo is not a repo match"
        );
        assert!(
            !candidate_hosts_repo(&candidate, &[]),
            "no registered dashboard for this repo means no preference at all"
        );
    }

    /// Issue #620, behaviour 1: the dashboard HOSTING this caller wins, even
    /// when a more recently started dashboard for another repository would
    /// win the machine-wide rule. `owner_pid` on the caller's own registry
    /// row names it, and that answer survives the seat's row being re-created
    /// by a restart with handoff.
    #[test]
    fn the_dashboard_hosting_this_caller_is_preferred_over_a_newer_foreign_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));

        // The caller's own registry row, exactly as a dashboard-hosted seat
        // re-registers it after a restart: verb `chat`, and `owner_pid` the
        // dashboard's (this process, here).
        let session = "e84d72bc-1111-4222-8333-444444444444";
        let record = super::super::super::sessions::Record::new(
            session,
            "claude",
            tmp.path(),
            super::super::super::sessions::Verb::Chat,
        );
        let _guard = super::super::super::sessions::SessionGuard::register(&state, record);

        let hosting = state.dash().join("aaaa1111-hosting").join("requests");
        let foreign = state.dash().join("bbbb2222-foreign").join("requests");
        let base = std::time::SystemTime::now();
        let candidates = vec![
            // Newer, so it wins `select_live_dash_dir` outright -- and it is
            // the foreign-repo dashboard from the bug report.
            super::super::super::dash::DashCandidate {
                requests_dir: foreign.clone(),
                status: super::super::super::dash::CandidateStatus::Live {
                    started_at: base + std::time::Duration::from_secs(60),
                    pid: crate::commands::ctx::testenv::dead_pid(),
                },
            },
            super::super::super::dash::DashCandidate {
                requests_dir: hosting.clone(),
                status: super::super::super::dash::CandidateStatus::Live {
                    started_at: base,
                    pid: std::process::id(),
                },
            },
        ];

        let mut env = base_env(state.root());
        env.insert(
            super::super::super::adapters::SESSION_ENV.to_string(),
            session.to_string(),
        );
        let chosen = select_join_target(&state, &candidates, tmp.path(), &|k| env.get(k).cloned())
            .expect("a live dashboard is selected");
        assert_eq!(
            chosen.requests_dir, hosting,
            "the dashboard this seat is hosted by must win over a newer foreign one"
        );

        // With no session identity at all the machine-wide rule is unchanged.
        let plain = base_env(state.root());
        let chosen =
            select_join_target(&state, &candidates, tmp.path(), &|k| plain.get(k).cloned())
                .expect("a live dashboard is still selected");
        assert_eq!(chosen.requests_dir, foreign);
    }

    /// Issue #620, behaviour 2: a dashboard-hosted seat re-registered as
    /// `verb = chat` still makes its dashboard a repo match. The token
    /// directory is named by the dashboard's short id, which IS that seat's
    /// short id -- keying the repo preference on `Verb::Dash` alone lost the
    /// live dashboard the moment the seat was restarted.
    #[test]
    fn a_restarted_hosted_seats_chat_record_still_names_its_dashboard_for_this_repo() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let session = "e84d72bc-1111-4222-8333-444444444444";
        let short = super::super::super::sessions::short_id(session);
        let record = super::super::super::sessions::Record::new(
            session,
            "claude",
            tmp.path(),
            super::super::super::sessions::Verb::Chat,
        );
        let _guard = super::super::super::sessions::SessionGuard::register(&state, record);

        let shorts = dash_shorts_for_repo(&state, tmp.path());
        assert!(
            shorts.contains(&short),
            "the hosted seat's own row names its dashboard for this repo: {shorts:?}"
        );
        let candidate = super::super::super::dash::DashCandidate {
            requests_dir: state
                .dash()
                .join(format!("{short}-0123456789abcdef"))
                .join("requests"),
            status: super::super::super::dash::CandidateStatus::NoOwnerPid,
        };
        assert!(candidate_hosts_repo(&candidate, &shorts));
    }

    /// F2 (defense in depth): the request's prompt is encoded positionally
    /// into the pane's argv, so a prompt shaped like a flag would reach the
    /// real harness child as one. The dashboard refuses such a request at the
    /// authority side; this end refuses to even write it, and falls back to
    /// the headless path, where the prompt travels as the `-p <value>` data
    /// it is.
    #[test]
    fn a_prompt_that_begins_with_a_dash_is_never_written_as_a_spawn_request() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let args = joinable_args("claude", "--dangerously-skip-permissions");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
            None,
        );

        assert!(
            joined.is_inline(),
            "must fall through to the safe headless path"
        );
        assert!(out.is_empty(), "nothing is reported as spawned");
        let written: Vec<_> = std::fs::read_dir(&requests_dir)
            .expect("read requests dir")
            .flatten()
            .collect();
        assert!(
            written.is_empty(),
            "no request may be written at all: {written:?}"
        );
    }

    /// 2026-09-06: `--max-restarts`/`--timeout-secs`/`--max-tool-calls` and
    /// the trailing `-- flags` USED TO hard-error here, telling the operator
    /// to pass `--headless`. With headless gone as a spawn topology they all
    /// travel on the request instead, and the delegation still gets its
    /// visible pane.
    #[test]
    fn supervision_ceilings_travel_on_the_request_instead_of_hard_erroring() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"abcd1234","reason":null}"#)
        });

        let mut args = joinable_args("claude", "go");
        args.max_restarts = Some(2);
        args.timeout_secs = Some(90);
        args.max_tool_calls = Some(50);
        args.flags = vec!["--model".to_string(), "haiku".to_string()];

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("the delegation must reach the pane, not a hard error");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let req: spawnreq::SpawnRequest =
            serde_json::from_str(&request_body).expect("the request parses");
        assert_eq!(req.max_restarts, Some(2));
        assert_eq!(req.timeout_secs, Some(90));
        assert_eq!(req.max_tool_calls, Some(50));
        assert_eq!(req.flags, vec!["--model".to_string(), "haiku".to_string()]);
    }

    /// The F9 rule survives the removal: what a pane cannot hold exactly the
    /// way the inline path would is ANNOUNCED, never refused and never
    /// silently dropped. `--max-restarts`/`--timeout-secs` are absent because
    /// a pane genuinely honours both (it never restarts its child, and
    /// `dash::enforce_pane_deadlines` enforces the wall clock).
    #[test]
    fn a_pane_announces_only_the_ceilings_it_genuinely_cannot_hold() {
        let mut args = args_for("claude", "go");
        args.max_restarts = Some(2);
        args.timeout_secs = Some(90);
        args.max_tool_calls = None;
        args.flags = Vec::new();
        assert!(
            pane_ceiling_notices(&args).is_empty(),
            "a restart budget and a wall clock are both honoured on a pane"
        );

        args.max_tool_calls = Some(50);
        args.flags = vec!["--model".to_string(), "haiku".to_string()];
        let notices = pane_ceiling_notices(&args);
        assert_eq!(notices.len(), 1, "a lone model pin travels: {notices:?}");
        assert!(notices[0].contains("--max-tool-calls"), "got {notices:?}");

        args.flags = vec![
            "--model".to_string(),
            "haiku".to_string(),
            "--verbose".to_string(),
        ];
        let notices = pane_ceiling_notices(&args);
        assert_eq!(notices.len(), 2, "got {notices:?}");
        assert!(
            notices[1].contains("--verbose"),
            "the dropped flags must be named: {notices:?}"
        );
    }

    /// R1-7: the drop channel cannot tell an operator's terminal from a
    /// pane's own harness child (`dash::mod::intake_channels`: the shared
    /// `requests` leaf proves nothing, issue #179), so the dashboard clears
    /// `force` on every file drop -- including one an operator really did
    /// type. A demotion the operator cannot see is worse than either
    /// alternative, so it is announced.
    #[test]
    fn a_pane_announces_that_it_cannot_carry_an_operators_force() {
        let mut args = args_for("claude", "go");
        args.max_tool_calls = None;
        args.flags = Vec::new();
        args.force = false;
        assert!(
            pane_ceiling_notices(&args).is_empty(),
            "nothing was overridden, so there is nothing to say"
        );

        args.force = true;
        let notices = pane_ceiling_notices(&args);
        assert_eq!(notices.len(), 1, "got {notices:?}");
        assert!(
            notices[0].contains("--force"),
            "the dropped override must be named: {notices:?}"
        );
    }

    /// R5: an unanswered, unclaimed request is taken back off disk before the
    /// headless fallback starts. `take_requests` runs on the dashboard's own
    /// tick, so a request left behind could still be picked up afterwards --
    /// and then the headless run and that pane would both work the same
    /// prompt, which is exactly the double-run the claim protocol exists to
    /// prevent.
    #[test]
    fn an_unclaimed_timeout_takes_its_own_request_back_off_disk() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
            None,
        );
        assert!(joined.is_inline(), "nobody answered, so this runs headless");

        let leftover: Vec<PathBuf> = std::fs::read_dir(&requests_dir)
            .expect("read requests dir")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(
            leftover.is_empty(),
            "the request must not be left for a later tick to pick up: {leftover:?}"
        );
    }

    /// Issue #144: `sessions::nested_session_evidence` was hardened to treat
    /// a `DASH_REQUESTS_ENV` directory whose `owner.pid` names a dead process
    /// as no evidence of a live dashboard (see that module's own
    /// `only_a_live_dashboard_owner_pidfile_counts_as_a_dashboard_owner`),
    /// but `try_join_dashboard` was never updated to match: it only ever
    /// checked `dir.is_dir()`. A directory a crashed or force-quit dashboard
    /// left behind (a clean quit removes it; an abnormal exit does not) is
    /// therefore wrongly treated as a live channel by this side of the
    /// rendezvous -- a request is written into it, nobody is listening, and
    /// the caller burns the *entire* ack timeout finding that out. That is
    /// exactly the "dashboard did not answer" symptom the issue reports, and
    /// it fires even while some other, unrelated dashboard is genuinely
    /// running elsewhere.
    #[test]
    fn a_dead_dashboards_leftover_directory_is_refused_immediately() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());
        std::fs::write(
            requests_dir.parent().expect("parent").join("owner.pid"),
            crate::commands::ctx::testenv::dead_pid().to_string(),
        )
        .expect("write owner.pid");

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let started = std::time::Instant::now();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        );
        assert!(
            joined.is_inline(),
            "no live dashboard owns this directory; falls back to headless"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dead dashboard's leftover directory must be refused immediately, never waited \
             out against the full ack timeout"
        );
        assert!(
            std::fs::read_dir(&requests_dir)
                .expect("read requests dir")
                .flatten()
                .next()
                .is_none(),
            "no request may be written into a dead dashboard's directory at all"
        );
    }

    /// The other half of `dashboard_owner_liveness`'s three-way split: a
    /// requests directory with no `owner.pid` at all (rather than one naming
    /// a dead pid) must be refused exactly the same way -- immediately, with
    /// no request ever written. Distinct code path from the dead-pid test
    /// above (`OwnerLiveness::Missing` vs `OwnerLiveness::Dead`), so both are
    /// covered rather than assuming one implies the other.
    #[test]
    fn a_dashboard_directory_with_no_owner_pid_is_refused_immediately() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        // Deliberately not `live_dashboard_dir`: this is exactly the one
        // thing it always writes that this test needs absent.
        let requests_dir = tmp.path().join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");
        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            requests_dir.display().to_string(),
        );

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let started = std::time::Instant::now();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        );
        assert!(
            joined.is_inline(),
            "no owner.pid means no dashboard can be confirmed live; falls back to headless"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a missing owner.pid must be refused immediately, never waited out against the \
             full ack timeout"
        );
        assert!(
            std::fs::read_dir(&requests_dir)
                .expect("read requests dir")
                .flatten()
                .next()
                .is_none(),
            "no request may be written when no dashboard owner can be confirmed"
        );
    }

    /// F2: the request's removal is what decides which way an unanswered
    /// timeout goes, so a request that is no longer where this process left it
    /// is waited out as claimed -- even with no `claim-*` file to be seen. The
    /// old `is_claimed` pre-check read the claim a moment before acting on it,
    /// and a claim landing inside that window sent this side headless while the
    /// dashboard was already spawning the same prompt.
    #[test]
    fn a_request_that_vanished_without_a_claim_file_is_waited_out_not_double_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        // Moves the request aside to a name that is neither a request nor a
        // claim, and never acks: exactly what `is_claimed` would have read as
        // "nobody has this", and what `remove_file` reads as "not mine any
        // more".
        let taker = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let found = std::fs::read_dir(&dir)
                        .ok()
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .find(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("req-") && n.ends_with(".json"))
                        });
                    if let Some(path) = found {
                        std::fs::rename(&path, dir.join("taken-elsewhere")).expect("rename");
                        return;
                    }
                    if std::time::Instant::now() > deadline {
                        panic!("no spawn request appeared within the deadline");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(300),
            Duration::from_millis(300),
            None,
        );
        taker.join().expect("taker thread");

        let code = joined
            .expect_answer("a request this process could not take back must not run inline")
            .expect("writes its line");
        assert_eq!(code, 1, "an unconfirmed spawn is a failure, not a success");
        assert!(
            String::from_utf8_lossy(&out).contains("claimed the request but never confirmed"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Claims the next request exactly the way the dashboard's own tick does
    /// -- `take_requests`, which claims by renaming (O6) -- and then runs
    /// `respond` with its stem. Returns the request's raw contents.
    fn claim_next_request(dir: std::path::PathBuf, respond: impl Fn(&std::path::Path, &str)) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let taken = crate::commands::ctx::dash::spawnreq::take_requests(&dir);
            if let Some((path, _)) = taken.first() {
                let stem = crate::commands::ctx::dash::spawnreq::request_stem(path).expect("stem");
                respond(&dir, &stem);
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("no spawn request appeared within the deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// O3: a claim buys the dashboard extra time, not a free pass. When the
    /// extension runs out with no ack, the delegation fails honestly -- and
    /// still does not double-run headless, because the dashboard holds the
    /// claim and may yet spawn the pane.
    #[test]
    fn a_claimed_but_unconfirmed_request_fails_instead_of_reporting_success() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        // Takes (and so claims) the request, then deliberately never acks.
        let claimer = std::thread::spawn({
            let dir = requests_dir.clone();
            move || claim_next_request(dir, |_dir, _stem| {})
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(300),
            Duration::from_millis(300),
            None,
        );
        claimer.join().expect("claimer thread");

        let code = joined
            .expect_answer("a claimed request must not run inline")
            .expect("writes its line");
        assert_eq!(code, 1, "an unconfirmed spawn is a failure, not a success");
        let printed = String::from_utf8_lossy(&out);
        assert!(
            printed.contains("claimed the request but never confirmed")
                && printed.contains("zirv ctx status"),
            "got {printed}"
        );
    }

    /// O3, the other half: an ack that arrives late -- after the first
    /// timeout, inside the extension the claim bought -- is a success.
    #[test]
    fn a_late_ack_inside_the_claim_extension_still_succeeds() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                claim_next_request(dir, |dir, stem| {
                    // Past the requester's own first timeout, well inside the
                    // extension: a slow spawn, not a dead dashboard.
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    std::fs::write(
                        dir.join(format!("ack-{stem}.json")),
                        r#"{"ok":true,"short":"bbbb2222","reason":null}"#,
                    )
                    .expect("write ack");
                })
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_secs(5),
            None,
        );
        responder.join().expect("responder thread");

        let code = joined
            .expect_answer("claimed, then acked")
            .expect("writes its line");
        assert_eq!(code, 0);
        assert!(
            String::from_utf8_lossy(&out).contains("bbbb2222"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// Issue #620, behaviour 3: a `retryable` refusal from the first live
    /// dashboard (the foreign-repo one from the bug report) costs the
    /// delegation one round-trip and the NEXT live candidate -- not the whole
    /// delegation. Only after every live dashboard has declined does it run
    /// inline.
    #[test]
    fn a_foreign_repo_refusal_tries_the_next_live_dashboard_before_running_inline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // The inherited channel is live, and is tried first.
        let (first, env) = live_dashboard_dir(tmp.path());
        // A second live dashboard, under the state dir, never inherited.
        let second = tmp
            .path()
            .join("state")
            .join("dash")
            .join("bbbb2222-secondtoken")
            .join("requests");
        std::fs::create_dir_all(&second).expect("mkdir second");
        std::fs::write(
            second.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write owner.pid");

        let refuser = std::thread::spawn({
            let dir = first.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"this dashboard only spawns panes in its own repo","retryable":true}"#,
                )
            }
        });
        let accepter = std::thread::spawn({
            let dir = second.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"feed5678","reason":null}"#)
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            None,
        );
        refuser.join().expect("refuser thread");
        accepter.join().expect("accepter thread");

        let code = joined
            .expect_answer("the second live dashboard took it")
            .expect("writes its line");
        assert_eq!(code, 0);
        assert!(
            String::from_utf8_lossy(&out).contains("feed5678"),
            "the delegation must land in the next live dashboard, not inline: {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// O2: a refusal the dashboard itself marks `retryable` -- a repo
    /// mismatch, a pty that would not open -- says nothing about whether the
    /// task may run, and the headless path was never subject to it. The join
    /// declines rather than killing the delegation outright.
    #[test]
    fn a_retryable_refusal_falls_back_to_the_headless_path() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"this dashboard only spawns panes in its own repo","retryable":true}"#,
                )
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            None,
        );
        responder.join().expect("responder thread");

        assert!(
            joined.is_inline(),
            "a channel-level failure must not suppress the headless path"
        );
        assert!(out.is_empty(), "nothing is reported as spawned");
    }

    /// O2, the other class: a policy refusal ends the delegation. Falling back
    /// to headless would run a task this operator's own configuration just
    /// refused.
    #[test]
    fn a_policy_refusal_ends_the_delegation() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (requests_dir, env) = live_dashboard_dir(tmp.path());

        let responder = std::thread::spawn({
            let dir = requests_dir.clone();
            move || {
                respond_to_next_request(
                    dir,
                    r#"{"ok":false,"short":null,"reason":"claude is disabled by .zirv/.settings.toml","retryable":false}"#,
                )
            }
        });

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            None,
        );
        responder.join().expect("responder thread");

        let code = joined
            .expect_answer("a refusal is definitive")
            .expect("writes");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8_lossy(&out).contains("disabled"),
            "got {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// The other half of F10: nothing claimed it, so the timeout still falls
    /// back to headless exactly as before.
    #[test]
    fn an_unclaimed_timeout_still_falls_back_to_headless() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let (_requests_dir, env) = live_dashboard_dir(tmp.path());

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let joined = try_join_dashboard(
            &args,
            &args.prompt,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            Duration::from_millis(200),
            Duration::from_millis(200),
            None,
        );
        assert!(joined.is_inline());
        assert!(out.is_empty());
    }

    /// `DASH_REQUESTS_ENV` set but naming a directory that does not exist --
    /// the dashboard already quit and reaped it, or the value is stale --
    /// must fall straight through to the existing headless path with no
    /// notice printed (byte-for-byte the pre-Task-11 behavior for this
    /// shape), the same fake-agent-bin pattern every other "reached the real
    /// spawn attempt" test in this codebase uses to prove it without ever
    /// launching a real agent.
    #[test]
    fn dashboard_join_falls_through_to_headless_when_the_directory_is_missing() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let mut env: HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                tmp.path().join("state").display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            // Pacing off: with the claude exemption gone from
            // `has_no_usage_source`, this empty state dir would otherwise
            // make the gate write its one no-source skip line into `out`,
            // and this test's whole proof is that `out` stayed empty.
            ("ZIRV_CTX_PACE".to_string(), "false".to_string()),
        ]
        .into();
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            tmp.path()
                .join("no-such-requests-dir")
                .display()
                .to_string(),
        );

        let args = joinable_args("claude", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("the configured binary does not exist, so the headless spawn must fail");
        // Unlike `wrap`'s own pty-based spawn (which names the configured
        // binary in its error text -- see `chat.rs`'s equivalent test), the
        // headless path's plain `std::process::Command::spawn` failure is a
        // bare OS error with no program name in it at all. So the proof here
        // is what did NOT happen: `try_join_dashboard` short-circuits with
        // `Ok(_)` and a message written to `out` on every path it actually
        // takes (spawned, or refused) -- an `Err` with nothing ever written
        // to `out` means neither happened, i.e. this genuinely fell through
        // past the (missing) dashboard directory and into the real headless
        // spawn attempt, which is what failed.
        assert!(
            out.is_empty(),
            "a dashboard short-circuit always writes a line to `out`; nothing was written here"
        );
        let msg = err.to_string();
        assert!(!msg.is_empty(), "got an error with no message at all");
    }

    // Issue #145: dashboard discovery fallback. `try_join_dashboard` used to
    // give up the moment its own inherited `DASH_REQUESTS_ENV` directory was
    // absent or its owner dead, even when a perfectly live dashboard was
    // sitting right next to it under `<state>/dash/*` -- the shape left
    // behind by a dashboard restart, where a pane's own child shell still
    // carries the old, now-stale env value. These tests build that layout
    // directly under `<state>/dash/`, matching `StateDir::dash()`'s own
    // production form, rather than `live_dashboard_dir`'s arbitrary single
    // directory (which only ever stood in for the one inherited channel).

    #[test]
    fn a_dead_inherited_dashboard_falls_back_to_a_live_sibling() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");

        let old_requests = state_dir
            .join("dash")
            .join("aaaa1111-oldtoken")
            .join("requests");
        std::fs::create_dir_all(&old_requests).expect("mkdir old requests");
        std::fs::write(
            old_requests.parent().expect("parent").join("owner.pid"),
            crate::commands::ctx::testenv::dead_pid().to_string(),
        )
        .expect("write dead owner.pid");

        let new_requests = state_dir
            .join("dash")
            .join("bbbb2222-newtoken")
            .join("requests");
        std::fs::create_dir_all(&new_requests).expect("mkdir new requests");
        std::fs::write(
            new_requests.parent().expect("parent").join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("write live owner.pid");

        let mut env = base_env(&state_dir);
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            old_requests.display().to_string(),
        );

        let responder = std::thread::spawn({
            let dir = new_requests.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"cafe1234","reason":null}"#)
        });

        let args = joinable_args("claude", "delegated after a dashboard restart");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("falls back to the live sibling dashboard");
        let request_body = responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as cafe1234"),
            "got {output}"
        );
        assert!(
            request_body.contains("delegated after a dashboard restart"),
            "the request must actually land in the live sibling's own requests dir: \
             {request_body}"
        );
        assert!(
            std::fs::read_dir(&old_requests)
                .expect("read old requests dir")
                .flatten()
                .next()
                .is_none(),
            "the dead dashboard's own directory must stay untouched"
        );
    }

    #[test]
    fn two_live_siblings_pick_the_most_recently_started_dashboard() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");

        // The inherited directory is simply gone (e.g. the operator quit that
        // dashboard outright) -- both candidates below are genuine fallback
        // siblings, neither privileged by being "the inherited one".
        let inherited = state_dir
            .join("dash")
            .join("aaaa1111-goneinherited")
            .join("requests");

        let older = state_dir
            .join("dash")
            .join("bbbb2222-oldertoken")
            .join("requests");
        let newer = state_dir
            .join("dash")
            .join("cccc3333-newertoken")
            .join("requests");
        std::fs::create_dir_all(&older).expect("mkdir older");
        std::fs::create_dir_all(&newer).expect("mkdir newer");
        let older_owner = older.parent().expect("parent").join("owner.pid");
        let newer_owner = newer.parent().expect("parent").join("owner.pid");
        std::fs::write(&older_owner, std::process::id().to_string()).expect("write older owner");
        std::fs::write(&newer_owner, std::process::id().to_string()).expect("write newer owner");

        let base = std::time::SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&older_owner)
            .expect("open older owner.pid")
            .set_modified(base)
            .expect("set_modified older");
        std::fs::File::options()
            .write(true)
            .open(&newer_owner)
            .expect("open newer owner.pid")
            .set_modified(base + std::time::Duration::from_secs(5))
            .expect("set_modified newer");

        let mut env = base_env(&state_dir);
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            inherited.display().to_string(),
        );

        let responder = std::thread::spawn({
            let dir = newer.clone();
            move || respond_to_next_request(dir, r#"{"ok":true,"short":"feed5678","reason":null}"#)
        });

        let args = joinable_args("claude", "go to the newest dashboard");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("falls back to the most recently started sibling");
        responder.join().expect("responder thread");

        assert_eq!(code, 0);
        let output = String::from_utf8_lossy(&out);
        assert!(
            output.contains("spawned in dashboard as feed5678"),
            "the newer-mtime sibling must be the one selected: {output}"
        );
        assert!(
            std::fs::read_dir(&older)
                .expect("read older requests dir")
                .flatten()
                .next()
                .is_none(),
            "the older sibling must never receive a request when a newer one exists"
        );
    }

    /// The other half of issue #145: when nothing under `<state>/dash/*` is
    /// live either, the fallback gives up (`None`) exactly like before this
    /// issue's fix -- and the reasons behind that are structured data
    /// (`dash::DashCandidate`/`CandidateStatus`), not only an `eprintln` a
    /// test has no seam to observe.
    #[test]
    fn no_live_dashboard_anywhere_falls_back_to_headless_with_reasons_available() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state_dir = tmp.path().join("state");

        let dead = state_dir
            .join("dash")
            .join("aaaa1111-deadtoken")
            .join("requests");
        let ownerless = state_dir
            .join("dash")
            .join("bbbb2222-noownertoken")
            .join("requests");
        std::fs::create_dir_all(&dead).expect("mkdir dead");
        std::fs::create_dir_all(&ownerless).expect("mkdir ownerless");
        let dead_pid_value = crate::commands::ctx::testenv::dead_pid();
        std::fs::write(
            dead.parent().expect("parent").join("owner.pid"),
            dead_pid_value.to_string(),
        )
        .expect("write dead owner.pid");
        // `ownerless` deliberately gets no `owner.pid` at all.

        let inherited = state_dir
            .join("dash")
            .join("cccc3333-goneinherited")
            .join("requests");
        let env = base_env(&state_dir);

        let target = live_join_target(
            Some(inherited.as_path()),
            &|k| env.get(k).cloned(),
            std::path::Path::new("."),
        );
        assert!(
            target.is_none(),
            "no live dashboard exists anywhere, so the fallback must give up"
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir);
        let candidates = crate::commands::ctx::dash::discover_live_dash_dirs(&state);
        assert_eq!(
            candidates.len(),
            2,
            "both candidates are still reported, not silently dropped: {candidates:?}"
        );
        let dead_status = candidates
            .iter()
            .find(|c| c.requests_dir == dead)
            .expect("dead candidate present")
            .status;
        assert_eq!(
            dead_status,
            crate::commands::ctx::dash::CandidateStatus::DeadOwner(dead_pid_value)
        );
        let ownerless_status = candidates
            .iter()
            .find(|c| c.requests_dir == ownerless)
            .expect("ownerless candidate present")
            .status;
        assert_eq!(
            ownerless_status,
            crate::commands::ctx::dash::CandidateStatus::NoOwnerPid
        );
    }
}
