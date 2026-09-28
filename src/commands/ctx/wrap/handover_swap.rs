//! handover swap support for the interactive supervisor.

use super::*;

/// Issue #37: the clean-session-end companion to the pre-existing rot/
/// timeout-restart harvest call in the `Action::Restart` arm inside `pump`
/// below. Called from both places `pump` reports a genuinely clean
/// `SessionEnded` (a `try_wait` exit and a `PtyClosed` event) -- never from
/// the relaunch-failed exit further down, which already ran a harvest as
/// part of its own `Action::Restart` handling just above it: two harvest
/// model calls at the same boundary is exactly what issue #37's "one entry
/// point" rule forbids. Gated on `cfg.memory.harvest` here too, before the
/// transcript is even read, so an operator who left harvesting off never
/// pays for the read or either model call `memory::harvest_at_session_end`
/// can make. Best-effort: any failure is discarded, never surfaced to the
/// caller, so it can never turn a clean exit into a failed one.
#[allow(clippy::too_many_arguments)]
pub(super) fn harvest_at_clean_exit(
    adapter: &dyn AgentAdapter,
    transcript: &TranscriptSource,
    tail_items: usize,
    distiller_model: &str,
    distiller_timeout: Duration,
    repo: &Path,
    state_dir: &super::state::StateDir,
    memory_slug: &str,
    cfg: &CtxConfig,
) {
    if !cfg.memory.enabled || !cfg.memory.harvest {
        return;
    }
    let jsonl = transcript
        .path()
        .map(|path| std::fs::read_to_string(path).unwrap_or_default())
        .unwrap_or_default();
    let ctx = adapter.structural_context(&jsonl, tail_items);
    let _ = super::memory::harvest_at_session_end(
        adapter,
        distiller_model,
        &ctx,
        distiller_timeout,
        repo,
        state_dir,
        memory_slug,
        cfg,
    );
}

/// What a successful [`perform_handover_swap`] changed, for the ack and the
/// `zirv ▸` announcement -- both models named, matching the decision-log
/// entry's own contract (CLAUDE.md, "Record in the decision log with both
/// models named").
pub(super) struct HandoverOutcome {
    pub(super) from_agent: String,
    pub(super) from_model: String,
    pub(super) to_agent: String,
    pub(super) to_model: String,
    pub(super) stored: CtxResult<PathBuf>,
    pub(super) source: &'static str,
    pub(super) native: Option<super::dash::Pane>,
}

/// `zirv ctx wrap`'s successor backend.
///
/// Harness successors still swap onto the existing pty below. A native
/// successor is instead opened completely through the shared runtime seam,
/// retained here, and handed to the dashboard only after the old child has
/// exited and wrap has restored the real terminal. Admission checks the
/// release gate first, so a gated build keeps the source untouched and parks
/// exactly as it did before issue #632.
struct WrapSwapLauncher<'a> {
    session: &'a str,
    native_available: bool,
    native: Option<super::dash::Pane>,
    launch_native:
        &'a mut dyn FnMut(
            &super::rollover::runtime::SuccessorPlan,
        )
            -> Result<super::dash::Pane, super::rollover::runtime::SuccessorRefusal>,
}

impl super::rollover::runtime::SuccessorLauncher for WrapSwapLauncher<'_> {
    fn admits(
        &self,
        plan: &super::rollover::runtime::SuccessorPlan,
    ) -> Result<(), super::rollover::runtime::SuccessorRefusal> {
        match plan.to {
            super::runtime::RuntimeKind::Harness => Ok(()),
            super::runtime::RuntimeKind::Native if self.native_available && cfg!(unix) => Ok(()),
            super::runtime::RuntimeKind::Native => Err(
                super::rollover::runtime::SuccessorRefusal::LaunchFailed(format!(
                    "{}; the seat is parked on its current harness with its handoff stored",
                    super::runtime::NATIVE_COMING_SOON
                )),
            ),
            super::runtime::RuntimeKind::Unknown => {
                Err(super::rollover::runtime::SuccessorRefusal::LaunchFailed(
                    "`zirv ctx wrap` cannot launch an unknown successor runtime; the seat is \
                     parked on its current harness with its handoff stored"
                        .to_string(),
                ))
            }
        }
    }

    fn launch(
        &mut self,
        plan: &super::rollover::runtime::SuccessorPlan,
    ) -> Result<String, super::rollover::runtime::SuccessorRefusal> {
        if plan.to == super::runtime::RuntimeKind::Harness {
            return Ok(self.session.to_string());
        }
        let successor = (self.launch_native)(plan)?;
        let session = successor.session_id().to_string();
        self.native = Some(successor);
        Ok(session)
    }
}

/// Issue #84: swaps the orchestrator seat's model or harness in place,
/// mirroring `pump`'s own `Action::Restart` arm (distill via the existing
/// handoff machinery, quit the old child, open a fresh pty, relaunch) but
/// generalized to a possibly *different* adapter and model, resolved from
/// `req`. The caller (`pump`) has already decided this is a safe moment to
/// act (idle, or `--force`) and has already parked the registry record on
/// zirv's own pid.
///
/// On success, `*adapter`/`*distiller_model`/`*turn_env` are all updated in
/// place so every later tick of the same `pump` loop -- a subsequent
/// compact/quit sequence, a capabilities-gated mail delivery, a rot-
/// triggered restart -- runs against the new harness, not the old one.
/// `session_guard`/`session` are never touched here beyond `adopt_child_pid`:
/// the session keeps its existing registry short id throughout, which is
/// what makes mail sent before the swap still deliverable after it, and
/// `zirv ctx nudge` still resolve to the same address. `mail_watch` IS
/// touched, though, at the moment the writer sink is swapped: any owed
/// `\r` armed against the old child (`MailWatch::pending_submit`) is
/// cleared there, since the fresh successor never saw the text that CR
/// would submit.
///
/// The handoff packet rides the same channel every wrap restart already
/// uses -- the interactive launch's own positional/task prompt
/// (`relaunch_command`/`restart_prompt`), never a system-prompt injection --
/// so a successor with no system-prompt injection mechanism at all (codex)
/// receives it exactly the same way a same-harness restart already would.
#[allow(clippy::too_many_arguments)]
pub(super) fn perform_handover_swap(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    child_guard: &mut super::supervise::ChildGuard,
    session_guard: &mut super::sessions::SessionGuard,
    pair: &mut portable_pty::PtyPair,
    writer: &std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    mail_watch: &mut MailWatch,
    generation: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx: &mpsc::Sender<PumpEvent>,
    cpr_filter: &std::sync::Arc<std::sync::Mutex<CprFilter>>,
    bar: &mut BarRuntime,
    adapter: &mut Box<dyn AgentAdapter>,
    distiller_model: &mut String,
    turn_env: &mut Vec<(String, String)>,
    transcript: &mut TranscriptSource,
    server: Option<&super::signal::SignalServer>,
    session: &super::event::SessionId,
    repo: &Path,
    cfg: &CtxConfig,
    state_dir: &super::state::StateDir,
    role: PromptRole,
    memory_slug: &str,
    grace: Duration,
    tail_items: usize,
    distiller_timeout: Duration,
    last_size: (u16, u16),
    announcer: &Announcer,
    req: &super::handover::HandoverRequest,
) -> CtxResult<HandoverOutcome> {
    let from_agent = adapter.name().to_string();
    let from_model = turn_env
        .iter()
        .find(|(k, _)| k == adapters::SEAT_MODEL_ENV)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "default".to_string());

    let jsonl = transcript
        .path()
        .map(|path| std::fs::read_to_string(path).unwrap_or_default())
        .unwrap_or_default();
    let ctx = adapter.structural_context(&jsonl, tail_items);
    let previous = handoff::latest_for_repo(state_dir, repo)
        .ok()
        .flatten()
        .map(|(_, h)| h);
    // Issue #358: a reactive rollover fires precisely because this provider
    // has stopped answering, and the distiller call runs against that same
    // provider -- spending it could only ever time out and delay the swap.
    let (note, source) = if req.structural_only {
        (handoff::structural(&ctx), "structural")
    } else {
        handoff::distill_or_structural(
            adapter.as_ref(),
            distiller_model.as_str(),
            &ctx,
            distiller_timeout,
            announcer.enabled,
            previous.as_ref(),
        )
    };
    let stored = handoff::store(state_dir, repo, session.as_str(), &note);
    // N6: same rule the ordinary restart arm follows -- opt-in, and only
    // from a genuinely distilled handoff.
    if source == "distilled" {
        let _ = super::memory::harvest_durable(
            adapter.as_ref(),
            distiller_model.as_str(),
            &note,
            repo,
            state_dir,
            memory_slug,
            cfg,
        );
    }

    // Issue #552 (review round 1): WHICH RUNTIME the successor is, decided
    // before a harness adapter is resolved for it. Everything below this
    // point assumes a harness child; a rollover that chose a native route
    // used to arrive here and be swapped onto `req.target_agent` anyway.
    // Routed through the one seam every live swap shares, so this seat's
    // subagents are settled by the same `settle_subagents` a dashboard swap
    // runs -- and, because `admits` is asked first, NOT settled when the
    // swap is refused. Nothing has been torn down yet, so the `?` parks the
    // seat on its current harness with the handoff already stored.
    let successor_generation_for_plan = req
        .generation
        .or_else(|| super::seat::load(state_dir, &bar.session_short).map(|seat| seat.generation))
        .unwrap_or(1);
    let plan = super::rollover::runtime::plan_successor(
        super::runtime::RuntimeKind::Harness,
        req.successor_runtime(),
        &bar.session_short,
        successor_generation_for_plan,
        Some(&req.target_agent),
        req.target_model.as_deref(),
        req.target_route.as_deref(),
        req.resume_session.as_deref(),
        super::rollover::runtime::load(state_dir, &bar.session_short)
            .and_then(|record| record.boundary)
            .as_ref(),
    );
    let successor_verb = session_guard.record().verb;
    let mut launch_native = |plan: &super::rollover::runtime::SuccessorPlan| {
        super::rollover::runtime::launch_native_pane(
            cfg,
            state_dir,
            repo,
            repo,
            successor_verb,
            "orch".to_string(),
            last_size,
            role,
            plan,
            &note,
            &super::rollover::runtime::NativeSuccessorSpec::default(),
        )
    };
    let mut launcher = WrapSwapLauncher {
        session: session.as_str(),
        native_available: super::runtime::native_available(),
        native: None,
        launch_native: &mut launch_native,
    };
    super::rollover::runtime::launch_successor(
        state_dir,
        repo,
        &mut launcher,
        &plan,
        Some(session.as_str()),
        if req.structural_only {
            super::rollover::runtime::Drain::Forced
        } else {
            super::rollover::runtime::Drain::Quiesced
        },
        super::state::now_secs(),
    )
    .map_err(|refusal| refusal.to_string())?;

    if let Some(mut native) = launcher.native.take() {
        let quit = match writer.lock() {
            Ok(mut sink) => quit_child(&mut *sink, child, adapter.quit_sequence(), grace),
            Err(_) => Err("pty writer poisoned".into()),
        };
        if let Err(error) = quit {
            let _ = native.shutdown("");
            return Err(error);
        }
        generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        transcript.forget();
        return Ok(HandoverOutcome {
            from_agent,
            from_model,
            to_agent: super::runtime::RuntimeKind::Native.as_str().to_string(),
            to_model: req
                .target_model
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            stored,
            source,
            native: Some(native),
        });
    }

    // Everything from here on names the *new* harness. Resolved before the
    // old child is touched, so an unknown target agent (a race against the
    // operator's own config change, or a stale request) fails before
    // anything is torn down.
    // `relaunch` below always hands the successor the handoff packet as its
    // initial prompt, so this launch can only resume a conversation on a
    // harness that accepts both.
    let (new_adapter, new_extra_flags) =
        super::handover::resolve_swap_launch(cfg, req, true, role)?;
    // Finding #10 (issue #358 review): the successor must carry a fencing
    // generation of its own. `req.generation` is the PREPARED generation an
    // automatic swap's `seat::commit` is about to promote to `Seat::
    // generation`; a manual swap (`req.generation: None`) opens no
    // transaction and never changes the seat's generation at all, so it
    // falls back to whatever is on disk right now.
    let successor_generation = req
        .generation
        .or_else(|| super::seat::load(state_dir, &bar.session_short).map(|seat| seat.generation));
    let new_turn_env = super::handover::build_turn_env(
        new_adapter.as_ref(),
        server,
        session.as_str(),
        repo,
        role,
        req.target_model.as_deref(),
        successor_generation,
    );

    let (new_generation, quit) = match writer.lock() {
        Ok(mut sink) => {
            let bumped = generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let quit = quit_child(&mut *sink, child, adapter.quit_sequence(), grace);
            (Some(bumped), quit)
        }
        Err(_) => (None, Err("pty writer poisoned".into())),
    };
    // That session is over, same as an ordinary restart: whatever it was
    // writing is now a dead file, and the successor reports its own on its
    // first turn.
    transcript.forget();
    // R6: the successor keeps this session's id, so codex's rollout pin would
    // otherwise keep answering the dead child's file for the rest of the run.
    super::adapters::codex::forget_transcript_pin(
        state_dir,
        &super::sessions::short_id(session.as_str()),
        super::state::now_secs(),
    );
    quit?;
    let new_generation = new_generation.ok_or("generation not bumped; pty writer poisoned")?;

    let (fresh_pair, fresh_child, fresh_reader, fresh_writer) = relaunch(
        new_adapter.as_ref(),
        repo,
        &note,
        &new_extra_flags,
        &new_turn_env,
        relaunch_size(bar, last_size),
        &cfg.screen.thresholds(),
        state_dir,
        session.as_str(),
    )?;
    spawn_output_thread(
        fresh_reader,
        tx.clone(),
        generation.clone(),
        new_generation,
        bar.stdout_lock.clone(),
    );
    if let Ok(mut sink) = writer.lock() {
        *sink = fresh_writer;
        // Issue #118 follow-up: a mail advisory's owed `\r` was armed
        // against the OLD child; the fresh successor never saw the text it
        // would submit, so it must not inherit the obligation either.
        mail_watch.clear_pending_submit();
    }
    if let Ok(mut filter) = cpr_filter.lock() {
        filter.arm(Instant::now());
    }
    *pair = fresh_pair;
    *child = fresh_child;
    // P1/P2/P3: released before the new adoption, same ordering the ordinary
    // restart arm uses, so a pid the OS has already recycled can never be
    // deregistered out from under the fresh child.
    child_guard.release();
    *child_guard = super::supervise::ChildGuard::adopt(child.process_id());
    if let Some(child_pid) = child.process_id() {
        session_guard.adopt_child_pid(child_pid);
    }

    let to_agent = new_adapter.name().to_string();
    let to_model = req
        .target_model
        .clone()
        .unwrap_or_else(|| "default".to_string());

    // Commit the swap: the boxed adapter and everything derived from it are
    // replaced together, so the rest of this `pump` loop's life runs against
    // the new harness consistently.
    *adapter = new_adapter;
    *distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    *turn_env = new_turn_env;
    bar.harness = adapter.name().to_string();
    bar.provider = adapter
        .provider_for_model(req.target_model.as_deref())
        .to_string();

    Ok(HandoverOutcome {
        from_agent,
        from_model,
        to_agent,
        to_model,
        stored,
        source,
        native: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Issue #632: once the native runtime is available, wrap uses the same
    /// pane launch backend as the dashboard instead of parking or silently
    /// selecting a harness swap.
    #[cfg(unix)]
    #[test]
    fn wrap_launches_a_native_successor_when_eligible_and_ungated() {
        use super::super::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = super::super::state::StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let note = Handoff {
            task: "carry the wrap seat forward".to_string(),
            ..Handoff::default()
        };
        let plan = super::super::rollover::runtime::plan_successor(
            RuntimeKind::Harness,
            RuntimeKind::Native,
            "aaaa1111",
            4,
            Some("claude"),
            None,
            None,
            None,
            None,
        );
        let native = super::super::rollover::runtime::NativeSuccessorSpec {
            writing: false,
            provider: Some(format!(
                "fixture:{}",
                super::super::runtime::fixture::fixture_root()
                    .join("helper-answer.json")
                    .display()
            )),
        };
        let mut open_native = |plan: &super::super::rollover::runtime::SuccessorPlan| {
            super::super::rollover::runtime::launch_native_pane(
                &cfg,
                &state,
                repo.path(),
                repo.path(),
                super::super::sessions::Verb::Chat,
                "orch".to_string(),
                (80, 24),
                PromptRole::Orchestrator,
                plan,
                &note,
                &native,
            )
        };
        let mut launcher = WrapSwapLauncher {
            session: "11111111-2222-4333-8444-555555555555",
            native_available: true,
            native: None,
            launch_native: &mut open_native,
        };
        let successor_session = super::super::rollover::runtime::launch_successor(
            &state,
            repo.path(),
            &mut launcher,
            &plan,
            Some("11111111-2222-4333-8444-555555555555"),
            super::super::rollover::runtime::Drain::Quiesced,
            1,
        )
        .expect("an ungated wrap seat launches its native successor");
        let mut successor = launcher.native.take().expect("a live native pane");
        assert!(successor.is_native());
        assert_eq!(successor.session_id(), successor_session);
        assert_eq!(successor.short(), plan.short);
        assert_eq!(
            super::super::seat::load(&state, &plan.short)
                .expect("successor seat")
                .generation,
            plan.generation
        );
        let _ = successor.shutdown("");
    }

    /// The release gate is checked at admission, before subagents are settled
    /// or the source pty is touched, so current builds retain park behaviour.
    #[test]
    fn wrap_parks_a_native_successor_while_the_release_gate_is_closed() {
        use super::super::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = super::super::state::StateDir::from_root(tmp.path().join("state"));
        let plan = super::super::rollover::runtime::plan_successor(
            RuntimeKind::Harness,
            RuntimeKind::Native,
            "aaaa1111",
            4,
            Some("claude"),
            None,
            None,
            None,
            None,
        );
        let launched = std::cell::Cell::new(false);
        let mut open_native = |_: &super::super::rollover::runtime::SuccessorPlan| {
            launched.set(true);
            Err(
                super::super::rollover::runtime::SuccessorRefusal::LaunchFailed(
                    "must not launch".to_string(),
                ),
            )
        };
        let mut launcher = WrapSwapLauncher {
            session: "11111111-2222-4333-8444-555555555555",
            native_available: false,
            native: None,
            launch_native: &mut open_native,
        };
        let refusal = super::super::rollover::runtime::launch_successor(
            &state,
            repo.path(),
            &mut launcher,
            &plan,
            Some("11111111-2222-4333-8444-555555555555"),
            super::super::rollover::runtime::Drain::Quiesced,
            1,
        )
        .expect_err("the gated native successor stays parked");
        let reason = refusal.to_string();
        assert!(
            reason.contains(super::super::runtime::NATIVE_COMING_SOON),
            "the refusal preserves the release gate: {reason}"
        );
        assert!(
            reason.contains("parked"),
            "the source remains parked rather than swapped: {reason}"
        );
        assert!(!launched.get(), "admission must refuse before launch");
        assert!(launcher.native.is_none());
    }
}
