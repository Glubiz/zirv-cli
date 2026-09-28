//! Stop-hook bookkeeping checkpoints: correction counts, compact-advisory
//! nudges, adoption records, and the modification/verify-on-stop gates.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::HookPayload;
use crate::commands::ctx::adapters::{self, SESSION_ENV};
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::event::{NormalizedEvent, input_hash};
use crate::commands::ctx::rot::{Score, Verdict};
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::supervise::Watcher;
use crate::commands::workflow::adoption::{self, AdoptionPolicy, AdoptionSignals};
#[cfg(test)]
use crate::commands::workflow::classify;
use crate::commands::workflow::skill::WorkflowPhase;
use crate::commands::workflow::{engine, telemetry, verification};

/// The optimize hint sentence, worded from the signal that actually fired
/// rather than always blaming the tools.
fn optimize_hint(reason: crate::commands::ctx::surface_collect::RecommendReason) -> &'static str {
    use crate::commands::ctx::surface_collect::RecommendReason;
    match reason {
        RecommendReason::ToolFailures => {
            "This session hit tools hard: `zirv ctx optimize` reviews the instruction files for \
             gaps behind repeated failures."
        }
        RecommendReason::Corrections => {
            "This session needed repeated corrections: `zirv ctx optimize` reviews the \
             instruction files for gaps behind that."
        }
    }
}

/// Decides what the Stop hook prints. `None` means print nothing, which is also
/// what every failure path does.
///
/// `adoption_nudge` (issue #223) rides along as an extra line: a session can
/// be perfectly `Healthy` by rot's own measure and still be doing substantial
/// edit work with no active `zirv workflow`, so it is folded into both the
/// healthy-session hint path and the ordinary advisory below, not gated
/// behind either. Despite the name, this parameter is generic "one more
/// advisory line" rather than exclusively about workflow adoption: `run_stop`
/// (issue #309) also folds its own verify-on-stop nudge in here rather than
/// widening this signature a second time.
///
/// `same_error_threshold` is `ScoreConfig::same_error_threshold` (default
/// `3`): when `score.signals.same_error_repeats` meets or exceeds it, the
/// advisory gets its own clause alongside the repetition one above -- a
/// stuck same-error loop is a distinct failure mode from over-verification
/// (the same tool call repeated with no edit in between): the fix is not
/// landing at all, not merely re-checked. A threshold of `0` is how an
/// operator disables the signal outright, so the clause also requires
/// `same_error_threshold > 0` -- otherwise `repeats >= 0` is trivially true
/// and the "disabled" signal would still print on every non-healthy advisory
/// (review finding F2). `stop_output` itself takes no `ScoreConfig` --
/// `run_stop`, its only production caller, already loads one and passes just
/// the threshold through.
pub fn stop_output(
    payload: &HookPayload,
    score: &Score,
    socket: Option<&Path>,
    optimize_recommended: Option<crate::commands::ctx::surface_collect::RecommendReason>,
    adoption_nudge: Option<&str>,
    same_error_threshold: usize,
) -> Option<String> {
    if payload.stop_hook_active {
        return None;
    }
    if socket.is_some() {
        return None;
    }
    if score.verdict == Verdict::Healthy
        && optimize_recommended.is_none()
        && adoption_nudge.is_none()
    {
        return None;
    }

    // A healthy session is never told to /compact or resume: the only thing
    // worth saying is the optimize hint (and adoption nudge, if any) that got
    // it here in the first place.
    if score.verdict == Verdict::Healthy {
        let mut message = optimize_recommended
            .map(optimize_hint)
            .unwrap_or_default()
            .to_string();
        if let Some(nudge) = adoption_nudge {
            if !message.is_empty() {
                message.push('\n');
            }
            message.push_str(nudge);
        }
        return serde_json::to_string(&serde_json::json!({ "systemMessage": message })).ok();
    }

    let mut advisory = format!(
        "zirv ctx: verdict {} (score {}, context {} tokens). Consider /compact, or run `zirv ctx resume` for a clean session with a handoff.",
        score.verdict.as_str(),
        score.score,
        score.context_tokens
    );
    // Over-verification: the same tool call fired repeatedly with no
    // edit-like call in between (`rot::repetition`'s own interleave-aware
    // rule) is a distinct failure mode from an ordinary rotted session --
    // re-running an unchanged check cannot produce a new result, so the
    // advisory says that plainly rather than just "consider /compact".
    if score.signals.repetition_hits > 0 {
        advisory.push(' ');
        advisory.push_str(&format!(
            "Same tool call repeated {}x with no edit in between: the result will not change, move on.",
            score.signals.max_repeat
        ));
    }
    // Same-error loop: the longest run of consecutive identical (normalized)
    // tool-result errors within the window met or crossed the operator's own
    // threshold -- a distinct failure mode from the repetition clause above,
    // which fires on an unchanged tool call rather than a recurring error.
    // `same_error_threshold > 0` is required too: a threshold of zero is how
    // an operator disables the signal, and `repeats >= 0` is trivially true,
    // so without this guard a disabled signal would still fire on every
    // non-healthy advisory (review finding F2).
    if same_error_threshold > 0 && score.signals.same_error_repeats >= same_error_threshold {
        advisory.push(' ');
        advisory.push_str(&format!(
            "Same error {}x in a row across different attempts: the fix isn't landing, try a different approach.",
            score.signals.same_error_repeats
        ));
    }
    if let Some(reason) = optimize_recommended {
        advisory.push(' ');
        advisory.push_str(optimize_hint(reason));
    }
    if let Some(nudge) = adoption_nudge {
        advisory.push('\n');
        advisory.push_str(nudge);
    }
    serde_json::to_string(&serde_json::json!({ "systemMessage": advisory })).ok()
}

/// Bumped whenever this file's shape changes, mirroring `score.rs`'s own
/// `CHECKPOINT_VERSION` pattern: an older file is discarded and rebuilt once
/// from scratch rather than misread.
const CORRECTION_CHECKPOINT_VERSION: u32 = 1;

/// Incremental cursor + running total for `corrections_in`, one file per
/// transcript (mirrors `score.rs`'s own per-transcript `checkpoint_path`).
///
/// `corrections_in` used to `read_to_string` and re-`structural_context` the
/// WHOLE transcript on every Stop hook call once a session passed the
/// correction-recommendation gate (`surface_collect::recommendation_possible`) --
/// O(session) per turn, O(n^2) over a session, exactly the cost the cached
/// score above already pays once to avoid for the rot score itself. This is
/// kept as its own small checkpoint rather than folded into `score.rs`'s
/// `Checkpoint` (a lower-level, adapter-agnostic scoring cursor that should
/// not grow an optimize-specific concept) or into `AdoptionRecord` above
/// (whose fold only runs when workflow-adoption policy is not `Off` and the
/// session is not a delegated worker -- neither gate has anything to do with
/// whether an optimize recommendation is due, and folding in there would
/// stop counting corrections for exactly the sessions where adoption nudges
/// are turned off).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CorrectionCheckpoint {
    #[serde(default)]
    version: u32,
    transcript: String,
    adapter: String,
    corrections: usize,
    offset: u64,
    consumed: u64,
}

fn correction_checkpoint_path(state: &StateDir, transcript: &Path) -> PathBuf {
    // Reuses `score.rs`'s own scoring directory (a different file per
    // transcript, distinguished by the `-corrections` suffix) rather than a
    // new state-dir root just for this.
    state.scoring().join(format!(
        "{:016x}-corrections.json",
        input_hash(&transcript.display().to_string())
    ))
}

/// `None` on any doubt at all -- unreadable, corrupt, a different schema
/// version, a different transcript, a different adapter, or an offset that no
/// longer fits the file -- which sends the caller back to a fresh fold from
/// byte zero, mirroring `score.rs::load_checkpoint`'s own guard.
fn load_correction_checkpoint(
    path: &Path,
    transcript: &Path,
    adapter_name: &str,
) -> Option<CorrectionCheckpoint> {
    let checkpoint: CorrectionCheckpoint =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let usable = checkpoint.version == CORRECTION_CHECKPOINT_VERSION
        && checkpoint.transcript == transcript.display().to_string()
        && checkpoint.adapter == adapter_name
        && checkpoint.offset <= std::fs::metadata(transcript).ok()?.len();
    usable.then_some(checkpoint)
}

/// Best-effort, like `score.rs::save_checkpoint`: a checkpoint that fails to
/// write costs the next Stop hook call a full re-fold, never a hook failure.
fn save_correction_checkpoint(path: &Path, checkpoint: &CorrectionCheckpoint) {
    let Ok(json) = serde_json::to_string(checkpoint) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// Corrections in `transcript`, read through the same adapter selection
/// `score_transcript` uses to score it (`cfg.agent`/`cfg.agent_bin`), not a
/// hardcoded claude parser (item 1). `adapters::select` failing (an unready
/// adapter, e.g. codex today) degrades to zero corrections rather than
/// panicking: an optimize recommendation is advisory, and a hook may never
/// fail loudly.
///
/// Incremental (see `CorrectionCheckpoint`'s own doc comment): only the bytes
/// appended to `transcript` since the last call are folded into the running
/// total, via the same `Watcher` cursor `fold_adoption_delta` above already
/// uses for `edit_like_calls`. Every adapter's `structural_context` parses
/// each JSONL line independently with no cross-line state (a line's own
/// `isSidechain`/`isMeta` flags decide its fate, nothing carried from an
/// earlier line), so folding a chunk of newly appended lines finds exactly
/// the correction-phrased user messages a full parse would have attributed
/// to those same lines -- the same property that already lets
/// `fold_adoption_delta` treat `adapter.parse_events` incrementally.
pub(super) fn corrections_in(state: &StateDir, transcript: &Path, cfg: &CtxConfig) -> usize {
    // Issue #690: `select_for_identity`, never `select` -- this names the
    // adapter whose transcript format to parse, and a hook subprocess with
    // a reduced `PATH` must not stop screening because a presence probe
    // could not see the harness that is running it.
    let Ok(adapter) = adapters::select_for_identity(cfg.agent.as_deref(), &[], cfg) else {
        return 0;
    };
    let path = correction_checkpoint_path(state, transcript);
    let mut checkpoint = load_correction_checkpoint(&path, transcript, adapter.name())
        .unwrap_or_else(|| CorrectionCheckpoint {
            version: CORRECTION_CHECKPOINT_VERSION,
            transcript: transcript.display().to_string(),
            adapter: adapter.name().to_string(),
            corrections: 0,
            offset: 0,
            consumed: 0,
        });

    let mut watcher = Watcher::resuming(
        transcript.to_path_buf(),
        checkpoint.offset,
        checkpoint.consumed,
    );
    let Ok(Some(appended)) = watcher.read_appended() else {
        // Nothing new to read (or the transcript vanished): the last
        // computed total still stands.
        return checkpoint.corrections;
    };
    if appended.restarted {
        checkpoint.corrections = 0;
    }
    checkpoint.corrections +=
        crate::commands::ctx::surface_collect::count_corrections(adapter.as_ref(), &appended.lines);
    let (offset, consumed) = watcher.position();
    checkpoint.offset = offset;
    checkpoint.consumed = consumed;
    save_correction_checkpoint(&path, &checkpoint);
    checkpoint.corrections
}

/// Bumped whenever this file's shape changes, mirroring `CorrectionCheckpoint`'s
/// own `CORRECTION_CHECKPOINT_VERSION` pattern.
const COMPACT_ADVISORY_CHECKPOINT_VERSION: u32 = 1;

/// Hook start-up overhead fix (wrapper-overhead benchmark, 2026-09-24): how
/// long a sampled `system_bytes`/`schema_bytes` pair (see [`CachedPromptBytes`])
/// is trusted before [`compact_advisory_stop_nudge`] recompiles the prompt to
/// resample it. A live measurement found `compile::compile_with_harness_
/// roster` averaging ~230ms per Stop hook call, almost entirely
/// `harness_roster_lines`' own per-adapter `AgentAdapter::ready()` calls (a
/// `resolve_program` PATH walk for every registered adapter, ~14 of them) --
/// NOT covered by that function's own `ProbeCache` (which only memoizes the
/// separate `liveness_probe` check, not `ready()` itself), so it paid this
/// cost fresh on every single turn. Mirrors [`crate::commands::ctx::adapters::
/// ProbeCache`]'s own `PROBE_CACHE_TTL_SECS`: the harness roster's byte size
/// is driven by the exact same "which harnesses are installed" fact that
/// cache already tolerates up to an hour stale.
const COMPACT_ADVISORY_PROMPT_BYTES_TTL_SECS: u64 = 3600;

/// A sampled `(system_bytes, schema_bytes)` pair from `compile::
/// compile_with_harness_roster`, plus when it was taken -- see
/// [`COMPACT_ADVISORY_PROMPT_BYTES_TTL_SECS`] for why this is cached rather
/// than resampled on every Stop hook call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CachedPromptBytes {
    system_bytes: u64,
    schema_bytes: Option<u64>,
    sampled_at: u64,
}

/// Issue #312: the reclaim-gated compact advisory's own persisted state, one
/// file per transcript (mirrors `CorrectionCheckpoint`). `accumulator` is
/// `breakdown::BreakdownAccumulator`, folded incrementally the same way
/// `AdoptionRecord::edit_like_calls` is -- UNBOUNDED, unlike `RotState`'s
/// windowed segments, because a stale-marking edit can reference a path read
/// arbitrarily many turns back (see that type's own doc comment).
/// `last_fired_window_tokens` is the hysteresis: `None` until the advisory
/// has fired once, then the window size (`Score::context_tokens`) it fired
/// at, so it cannot refire until the window has regrown a full
/// trigger-sized runway past that point -- mirroring Hermes's own
/// disarm-until-regrowth rule (see the issue's Origin section), reimplemented
/// here as advice rather than automatic pruning. `cached_prompt_bytes` is the
/// hook start-up overhead fix's own cache -- `#[serde(default)]` so a
/// checkpoint written before this field existed just resamples once, exactly
/// like a fresh checkpoint would.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CompactAdvisoryCheckpoint {
    #[serde(default)]
    version: u32,
    transcript: String,
    adapter: String,
    #[serde(default)]
    accumulator: crate::commands::ctx::breakdown::BreakdownAccumulator,
    offset: u64,
    consumed: u64,
    #[serde(default)]
    last_fired_window_tokens: Option<u64>,
    #[serde(default)]
    cached_prompt_bytes: Option<CachedPromptBytes>,
}

fn compact_advisory_checkpoint_path(state: &StateDir, transcript: &Path) -> PathBuf {
    // Reuses `score.rs`'s own scoring directory, like `correction_checkpoint_
    // path` right above.
    state.scoring().join(format!(
        "{:016x}-compact-advisory.json",
        input_hash(&transcript.display().to_string())
    ))
}

/// `None` on any doubt at all -- unreadable, corrupt, a different schema
/// version, a different transcript, a different adapter, or an offset that no
/// longer fits the file -- which sends the caller back to a fresh fold from
/// byte zero, mirroring every other checkpoint loader in this crate.
fn load_compact_advisory_checkpoint(
    path: &Path,
    transcript: &Path,
    adapter_name: &str,
) -> Option<CompactAdvisoryCheckpoint> {
    let checkpoint: CompactAdvisoryCheckpoint =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let usable = checkpoint.version == COMPACT_ADVISORY_CHECKPOINT_VERSION
        && checkpoint.transcript == transcript.display().to_string()
        && checkpoint.adapter == adapter_name
        && checkpoint.offset <= std::fs::metadata(transcript).ok()?.len();
    usable.then_some(checkpoint)
}

/// Best-effort, like every other checkpoint writer in this file: a checkpoint
/// that fails to write costs the next Stop hook call a full re-fold, never a
/// hook failure.
fn save_compact_advisory_checkpoint(path: &Path, checkpoint: &CompactAdvisoryCheckpoint) {
    let Ok(json) = serde_json::to_string(checkpoint) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// `~18k`-style rounding for the advisory's own reclaim figure -- plain
/// tokens below 1000, otherwise truncated to the nearest thousand. Cosmetic
/// only: the gate itself always compares the exact token count.
fn approx_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{}k", tokens / 1000)
    } else {
        tokens.to_string()
    }
}

/// Issue #312: the reclaim-gated compact advisory -- a SECOND, cost-driven
/// tier alongside the rot `Verdict` ladder `stop_output` already renders,
/// firing only when stale tool-result tokens exceed `compact_advisory.
/// min_reclaim_tokens` AND the window exceeds `compact_advisory.
/// window_fraction` of the model's resolved context window. Independent of
/// `score.verdict`: `run_stop` folds this into the same `combined_nudge`
/// line the healthy-session early return in `stop_output` already honours,
/// so a `Healthy`-verdict session with a lot of stale tool output still gets
/// told.
///
/// A real compaction (`NormalizedEvent::Compaction` among the newly
/// appended events) resets both the accumulator and the hysteresis: the
/// bytes it summarized are no longer live context, and "regrown a full
/// trigger-sized runway" must count from the post-compaction window, not a
/// stale pre-compaction one.
///
/// Samples the compiled-prompt bytes (`compile::compile_with_harness_
/// roster`), like `zirv ctx status --breakdown` does -- but, since the
/// wrapper-overhead benchmark (2026-09-24), no more than once per
/// [`COMPACT_ADVISORY_PROMPT_BYTES_TTL_SECS`]. A live measurement found that
/// compile averaging ~230ms per call, almost entirely `harness_roster_
/// lines`' own uncached per-adapter `ready()` PATH walk -- paid fresh on
/// EVERY Stop hook of EVERY turn for a number this advisory only needs
/// approximately right. `checkpoint.cached_prompt_bytes` (see
/// [`CachedPromptBytes`]) carries the last sample forward across calls; a
/// resample can therefore disagree with `status --breakdown`'s own live
/// number by up to that TTL, which this advisory's own imprecise, threshold-
/// gated wording ("~N tokens ... saves more than it costs") already assumes.
/// The accumulator fold above stays exactly as incremental as before this
/// fix -- only the prompt-bytes sample gained a cache.
///
/// `None` on every failure path and whenever either gate is not met -- like
/// every other hook advisory, this must never fail loudly.
pub(super) fn compact_advisory_stop_nudge(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    score: &Score,
    transcript: &Path,
    adapter: &dyn adapters::AgentAdapter,
) -> Option<String> {
    let path = compact_advisory_checkpoint_path(state, transcript);
    let mut checkpoint = load_compact_advisory_checkpoint(&path, transcript, adapter.name())
        .unwrap_or_else(|| CompactAdvisoryCheckpoint {
            version: COMPACT_ADVISORY_CHECKPOINT_VERSION,
            transcript: transcript.display().to_string(),
            adapter: adapter.name().to_string(),
            accumulator: crate::commands::ctx::breakdown::BreakdownAccumulator::default(),
            offset: 0,
            consumed: 0,
            last_fired_window_tokens: None,
            cached_prompt_bytes: None,
        });

    let mut watcher = Watcher::resuming(
        transcript.to_path_buf(),
        checkpoint.offset,
        checkpoint.consumed,
    );
    if let Ok(Some(appended)) = watcher.read_appended() {
        let events = adapter.parse_events(&appended.lines);
        match events
            .iter()
            .rposition(|event| matches!(event, NormalizedEvent::Compaction))
        {
            Some(boundary) => {
                checkpoint.accumulator =
                    crate::commands::ctx::breakdown::BreakdownAccumulator::default();
                checkpoint.last_fired_window_tokens = None;
                checkpoint.accumulator.feed_all(&events[boundary + 1..]);
            }
            None => checkpoint.accumulator.feed_all(&events),
        }
        let (offset, consumed) = watcher.position();
        checkpoint.offset = offset;
        checkpoint.consumed = consumed;
    }

    let model_window = cfg
        .score
        .model_context_tokens
        .or(adapter.capabilities_for_model(None).context_window_tokens);
    let advisory = model_window
        .filter(|window| *window > 0)
        .and_then(|window| {
            let now = now_secs();
            let fresh = checkpoint
                .cached_prompt_bytes
                .as_ref()
                .filter(|cached| {
                    now.saturating_sub(cached.sampled_at) < COMPACT_ADVISORY_PROMPT_BYTES_TTL_SECS
                })
                .cloned();
            let (system_bytes, schema_bytes) = match fresh {
                Some(cached) => (cached.system_bytes, cached.schema_bytes),
                None => {
                    let home = crate::utils::home_dir().ok();
                    let compiled = crate::commands::ctx::compile::compile_with_harness_roster(
                        home.as_deref(),
                        repo,
                        false,
                        cfg,
                        adapter,
                        crate::commands::ctx::prompt::PromptRole::Orchestrator,
                        state,
                        now,
                        true,
                        adapters::LaunchMode::Interactive,
                        true,
                    );
                    let system_bytes = compiled
                        .composed
                        .as_ref()
                        .map_or(0, |composed| composed.text.len() as u64);
                    let schema_bytes = compiled
                        .harness_roster
                        .as_ref()
                        .map(|roster| roster.delivered_bytes as u64);
                    checkpoint.cached_prompt_bytes = Some(CachedPromptBytes {
                        system_bytes,
                        schema_bytes,
                        sampled_at: now,
                    });
                    (system_bytes, schema_bytes)
                }
            };
            let summary = checkpoint.accumulator.materialize(
                score.context_tokens,
                system_bytes,
                schema_bytes,
            );

            if summary.tool_results_stale < cfg.compact_advisory.min_reclaim_tokens {
                return None;
            }
            let window_fraction = score.context_tokens as f64 / window as f64;
            if window_fraction < cfg.compact_advisory.window_fraction {
                return None;
            }
            let trigger_tokens = (cfg.compact_advisory.window_fraction * window as f64) as u64;
            if let Some(last_fired) = checkpoint.last_fired_window_tokens
                && score.context_tokens < last_fired.saturating_add(trigger_tokens)
            {
                return None;
            }

            checkpoint.last_fired_window_tokens = Some(score.context_tokens);
            let source = summary.stale_source.as_deref().unwrap_or("tool-result");
            Some(format!(
                "zirv ctx: ~{} tokens are stale `{source}` output; /compact now saves more than it \
             costs. Park bulk tool output on disk going forward.",
                approx_tokens(summary.tool_results_stale)
            ))
        });

    save_compact_advisory_checkpoint(&path, &checkpoint);
    advisory
}

/// `CtxConfig::load`'s degrade-on-error fallback used by the Stop hook's
/// optimize-recommendation path: a hook must never fail outright on a bad
/// config, but degrading all the way to `CtxConfig::default()` would hand
/// `corrections_in` a fully permissive `AgentGate`, which is exactly the
/// same trust hole `surface_collect.rs`'s config-load fallback had (review finding
/// 1): a malformed *repo* `.settings.toml` would silently revive an agent
/// the *operator* disabled. It would also, since issue #44 made `cfg.policy`
/// load-bearing, hand back the widest possible policy from a config that
/// could not even be read. `config::degrade_to_operator_only` substitutes
/// `AgentGate::load_operator_only`/`EffectivePolicy::fail_closed` for those
/// two fields, keeping both the operator's disable and the operator's policy
/// in force even when the rest of the config (or the repo settings layer
/// specifically) cannot be read.
pub(super) fn cfg_or_operator_only_gate(repo: &Path, env: EnvLookup<'_>) -> CtxConfig {
    match CtxConfig::load(repo, env) {
        Ok(cfg) => cfg,
        Err(_) => crate::commands::ctx::config::degrade_to_operator_only(env),
    }
}

/// Issue #223: per-session workflow-adoption bookkeeping, refreshed on every
/// Stop/Notify hook call and re-read (never re-scanned) by the Prompt hook.
/// `edit_like_calls`/`turns` are the same cumulative counts
/// `adoption::signals` would report over the whole transcript;
/// `offset`/`consumed` are this record's own [`Watcher`] resume position, so
/// a fresh hook-per-turn process still only ever parses the bytes appended
/// since the last one -- the same append-only-cost property `score.rs`'s own
/// incremental checkpoint has, kept as a separate small fold here rather than
/// widening that (separately versioned, heavily depended-on) schema.
///
/// `skill_loads` is the identical kind of cumulative count as
/// `edit_like_calls` (folded the same way, in the same pass), and
/// `last_skill_nudged_turn` is the skill nudge's own cadence field, kept
/// separate from `last_nudged_turn` so the workflow-adoption nudge and the
/// skill nudge never suppress each other. Both are `#[serde(default)]`: a
/// record persisted before this change simply reads back as "no loads seen,
/// never nudged yet", never a parse failure.
///
/// `shell_skill_loads` counts a shell-invoked `zirv skill load` -- the
/// PRIMARY load path (the standing skill index and the subagent skill pointer
/// both tell an agent to run it from a shell),
/// which `adoption::signals`'s transcript scan can never see (see that
/// module's own doc comment). Bumped ONLY by [`record_shell_skill_load`],
/// NEVER by [`fold_adoption_delta`]: it is not transcript-derived at all, so
/// a restarted transcript must never reset it the way
/// `edit_like_calls`/`skill_loads` are reset. A lower bound, not an exact
/// count: the bump is an unlocked read-modify-write, so two concurrent loads
/// may record one -- harmless, since the nudge only tests for zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AdoptionRecord {
    pub(crate) substantial: bool,
    pub(crate) edit_like_calls: usize,
    pub(crate) turns: usize,
    workflow_active: bool,
    first_detected_turn: Option<usize>,
    pub(super) last_nudged_turn: Option<usize>,
    detected_recorded: bool,
    recovered_recorded: bool,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    consumed: u64,
    #[serde(default)]
    pub(crate) skill_loads: usize,
    #[serde(default)]
    pub(super) last_skill_nudged_turn: Option<usize>,
    #[serde(default)]
    pub(crate) shell_skill_loads: usize,
}

/// One file per session id, named after a hash of it (mirrors `score.rs`'s
/// `checkpoint_path`): session ids are not always filesystem-safe on their
/// own, and are far too long/variable-shaped across adapters to trust as a
/// filename directly.
///
/// `pub(crate)`: `agent::run_with`'s own enforce-policy gate (issue #223 §E)
/// reads the same record this hook writes, rather than keeping a second copy
/// of this path/schema.
pub(crate) fn adoption_record_path(state: &StateDir, session: &str) -> std::path::PathBuf {
    state
        .adoption()
        .join(format!("{:016x}.json", input_hash(session)))
}

/// `Default` (nothing detected yet) on any doubt at all -- missing, corrupt,
/// unreadable -- exactly like every other best-effort state read a hook makes.
pub(crate) fn load_adoption_record(path: &Path) -> AdoptionRecord {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

/// Test-only, cross-module (`agent::run_with`'s enforce-gate tests build a
/// record directly rather than driving a whole Stop hook call): a record
/// already past the substantial threshold.
#[cfg(test)]
impl AdoptionRecord {
    pub(crate) fn substantial_for_test(edit_like_calls: usize, turns: usize) -> Self {
        Self {
            substantial: true,
            edit_like_calls,
            turns,
            ..Self::default()
        }
    }
}

/// Best-effort, like `score.rs`'s `save_checkpoint`: a record that fails to
/// write costs the next hook call a full-session refold, never a hook failure.
///
/// `pub(crate)`: also used directly by `agent::run_with`'s enforce-gate tests
/// (issue #223 §E) to seed a record without driving a whole Stop hook call.
pub(crate) fn save_adoption_record(path: &Path, record: &AdoptionRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// Folds only the transcript bytes appended since `record`'s own resume
/// position into its cumulative `edit_like_calls`/`skill_loads` (the
/// identical fold, extended to the second count). A restarted transcript
/// (compaction, rewrite) restarts both from zero, the same rule `RotState`
/// applies to the score itself.
fn fold_adoption_delta(
    record: &mut AdoptionRecord,
    transcript: &Path,
    adapter: &dyn adapters::AgentAdapter,
) {
    if !adapter.capabilities().events {
        return;
    }
    let mut watcher = Watcher::resuming(transcript.to_path_buf(), record.offset, record.consumed);
    let Ok(Some(appended)) = watcher.read_appended() else {
        return;
    };
    if appended.restarted {
        record.edit_like_calls = 0;
        record.skill_loads = 0;
    }
    let delta = adoption::signals(&adapter.parse_events(&appended.lines));
    record.edit_like_calls += delta.edit_like_calls;
    record.skill_loads += delta.skill_loads;
    let (offset, consumed) = watcher.position();
    record.offset = offset;
    record.consumed = consumed;
}

/// Best-effort bump of the CURRENT session's shell-invoked skill-load count.
/// `zirv skill load` is the primary load path, but `adoption::signals` only
/// ever sees a shell tool call's NAME, never its command text, so a shell
/// load is invisible to the skill nudge's transcript scan. `skill::run_load`
/// runs inside the wrapped session's shell and inherits `SESSION_ENV`, so it
/// calls this once after a successful load. `shell_skill_loads` is not
/// transcript-derived, so `fold_adoption_delta` never touches or resets it.
///
/// No `SESSION_ENV` (an unsupervised load), no state directory, or an
/// unreadable record are silently ignored; returning nothing keeps `zirv
/// skill load`'s own output and exit code unaffected by construction.
pub(crate) fn record_shell_skill_load(env: EnvLookup<'_>) {
    let Some(session) = env(SESSION_ENV).filter(|s| !s.is_empty()) else {
        return;
    };
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    let path = adoption_record_path(&state, &session);
    let mut record = load_adoption_record(&path);
    record.shell_skill_loads += 1;
    save_adoption_record(&path, &record);
}

/// Workflow-adoption detection and Stop-hook nudge text, in one pass.
/// `None` whenever nothing should be added to the hook's own output -- the
/// policy is `off`, this session is a delegated worker, or no nudge is due --
/// which is also every failure path: like every other hook function, this
/// must never fail loudly.
///
/// Delegated workers are never nudged: only the top-level session a human is
/// actually looking at should be told to start a workflow. A worker
/// pane/headless child inherits [`crate::commands::ctx::agent::WORK_GROUP_ENV`] from its own
/// delegation lineage (see that constant's own doc comment); a top-level
/// interactive session never has it set. This is the one real "am I a
/// delegated worker" signal already wired into a spawned child's own process
/// env today -- `telemetry::TelemetryEvent::parent_session_id` exists as a
/// field but nothing in this codebase populates it yet.
///
/// The skill nudge (`adoption::skill_nudge_due`/
/// `skill_nudge_text`) rides the SAME transcript scan and the SAME
/// delegated-worker exemption above -- it is computed after both early
/// returns, so it is silent under `workflow.adoption == Off` (the fold never
/// runs, so `record.substantial`/`skill_loads` never update -- deliberately
/// NOT a second, independent gate on the skill nudge itself: see
/// `AdoptionPolicy`'s own doc comment on what this key governs) and for a
/// delegated worker, for the identical reason the workflow nudge is. Unlike
/// the workflow nudge, it is NOT further gated on `workflow.adoption >=
/// AdoptionPolicy::Nudge` (an `Advise`-level operator still gets it) and NOT
/// gated on `workflow_active` (a workflow being active does not mean a skill
/// was ever loaded) -- only on `cfg.prompt.skill_index`, the same switch that
/// turns off the standing skill-index system-prompt layer and the Change-A
/// dispatch pointer.
pub(super) fn adoption_stop_nudge(
    state: &StateDir,
    repo: &Path,
    session: &str,
    cfg: &CtxConfig,
    score: &Score,
    transcript: &Path,
    env: EnvLookup<'_>,
) -> Option<String> {
    if cfg.workflow.adoption == AdoptionPolicy::Off {
        return None;
    }
    if env(crate::commands::ctx::agent::WORK_GROUP_ENV)
        .filter(|v| !v.is_empty())
        .is_some()
    {
        return None;
    }

    let path = adoption_record_path(state, session);
    let mut record = load_adoption_record(&path);

    if let Ok(adapter) = adapters::select_for_identity(cfg.agent.as_deref(), &[], cfg) {
        fold_adoption_delta(&mut record, transcript, adapter.as_ref());
    }
    record.turns = score.signals.turns;
    let signals = AdoptionSignals {
        edit_like_calls: record.edit_like_calls,
        turns: record.turns,
        skill_loads: record.skill_loads,
    };
    record.substantial = adoption::is_substantial(&signals);
    record.workflow_active = engine::load_active(state, repo).ok().flatten().is_some();

    let telemetry_cfg = telemetry::TelemetryConfig::from_config(&cfg.workflow);
    if record.substantial && !record.detected_recorded {
        record.detected_recorded = true;
        if !record.workflow_active {
            record.first_detected_turn.get_or_insert(record.turns);
        }
        let mut event = telemetry::TelemetryEvent::new(telemetry::TelemetryKind::AdoptionDetected);
        event.session_id = Some(session.to_string());
        event.workflow_active = Some(record.workflow_active);
        let _ = telemetry::record(state, repo, &event, &telemetry_cfg);
    }
    if record.workflow_active && record.first_detected_turn.is_some() && !record.recovered_recorded
    {
        record.recovered_recorded = true;
        let mut event = telemetry::TelemetryEvent::new(telemetry::TelemetryKind::AdoptionRecovered);
        event.session_id = Some(session.to_string());
        event.workflow_active = Some(true);
        let _ = telemetry::record(state, repo, &event, &telemetry_cfg);
    }

    let due = if cfg.workflow.adoption >= AdoptionPolicy::Nudge {
        adoption::nudge_due(
            cfg.workflow.adoption,
            record.substantial,
            record.workflow_active,
            record.turns,
            record.last_nudged_turn,
        )
    } else {
        // `Advise`: fires exactly once, the turn substantial-without-workflow
        // first becomes true. `nudge_due` itself never fires below `Nudge`
        // (see its own doc comment), so `Advise`'s single notice is decided
        // here instead.
        record.substantial && !record.workflow_active && record.last_nudged_turn.is_none()
    };
    let workflow_text = due.then(|| {
        record.last_nudged_turn = Some(record.turns);
        adoption::nudge_text(&signals, cfg.workflow.adoption)
    });

    // Own gate (`prompt.skill_index`), own due check, own cadence field --
    // see this function's own doc comment.
    // "zero loads" sums the transcript-derived count with the shell-invoked
    // one (`AdoptionRecord::shell_skill_loads`, bumped only by
    // `record_shell_skill_load`) -- `skill_nudge_due` itself stays pure and
    // unaware of where a load count came from.
    let skill_text = (cfg.prompt.skill_index
        && adoption::skill_nudge_due(
            record.substantial,
            record.skill_loads + record.shell_skill_loads,
            record.turns,
            record.last_skill_nudged_turn,
        ))
    .then(|| {
        record.last_skill_nudged_turn = Some(record.turns);
        adoption::skill_nudge_text(&signals)
    });

    save_adoption_record(&path, &record);
    let combined = [workflow_text, skill_text]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    (!combined.is_empty()).then(|| combined.join("\n"))
}

/// Issue #293: records ONE `TurnLatencySampled` sample for this scoring
/// pass, mirroring `adoption_stop_nudge`'s own local telemetry write right
/// next to it -- "the score is computed for a live session" is exactly this
/// call site, `run_stop`, and only here: the Stop hook is a fresh process on
/// every turn (`score_transcript_cached`'s own doc comment), so one call
/// here is one sample per turn, never per dashboard poll (`score::
/// cached_score`'s own fast path answers most of ITS polls from an
/// in-memory cache without ever reaching a scoring pass at all). `speed`
/// comes from `score::score_transcript_cached`'s third element
/// (`IncrementalScorer::last_speed_sample`) -- deliberately NOT a field on
/// `Score` itself, since it is only ever derived from this ONE poll's
/// appended events, not the whole session's accumulated history, and so is
/// legitimately allowed to differ between a bounded poll and a full parse
/// (unlike every field `Score` actually carries, which the incremental fold
/// and a full parse must always agree on). A no-op when `speed` is `None`
/// -- nothing measurable this pass, so nothing to record; best-effort like
/// every other telemetry write in this module (`let _ =
/// telemetry::record(..)`).
pub(super) fn record_speed_sample(
    state: &StateDir,
    repo: &Path,
    session: &str,
    cfg: &CtxConfig,
    speed: Option<crate::commands::ctx::event::SpeedMetrics>,
) {
    let Some(speed) = speed else {
        return;
    };
    let telemetry_cfg = telemetry::TelemetryConfig::from_config(&cfg.workflow);
    let mut event = telemetry::TelemetryEvent::new(telemetry::TelemetryKind::TurnLatencySampled);
    event.session_id = Some(session.to_string());
    event.turn_p50_ms = speed.turn_p50_ms;
    event.turn_max_ms = speed.turn_max_ms;
    event.ttft_p50_ms = speed.ttft_p50_ms;
    event.tool_error_rate = speed.tool_error_rate;
    let _ = telemetry::record(state, repo, &event, &telemetry_cfg);
}

/// Bumped whenever this file's own shape changes, mirroring `CorrectionCheckpoint`'s
/// own `CORRECTION_CHECKPOINT_VERSION`.
const MODIFICATION_CHECKPOINT_VERSION: u32 = 1;

/// Issue #309: incremental cursor plus a single "has this session made at
/// least one modification (edit-like) tool call" bit, one file per
/// transcript (mirrors `CorrectionCheckpoint`'s own naming/shape). Kept as
/// its own small checkpoint rather than folded into `AdoptionRecord`'s own
/// `edit_like_calls` fold: that fold only runs once `adoption_stop_nudge`
/// clears the `workflow.adoption != Off` gate, and verify-on-stop must keep
/// working when an operator has workflow-adoption nudges turned off but
/// still wants the stale-gate nudge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModificationCheckpoint {
    #[serde(default)]
    version: u32,
    transcript: String,
    adapter: String,
    modified: bool,
    offset: u64,
    consumed: u64,
}

fn modification_checkpoint_path(state: &StateDir, transcript: &Path) -> PathBuf {
    // Reuses `score.rs`'s own scoring directory, like `correction_checkpoint_
    // path` does, rather than a new state-dir root just for this.
    state.scoring().join(format!(
        "{:016x}-modified.json",
        input_hash(&transcript.display().to_string())
    ))
}

/// `None` on any doubt at all, mirroring `load_correction_checkpoint`'s own
/// guard.
fn load_modification_checkpoint(
    path: &Path,
    transcript: &Path,
    adapter_name: &str,
) -> Option<ModificationCheckpoint> {
    let checkpoint: ModificationCheckpoint =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if checkpoint.version != MODIFICATION_CHECKPOINT_VERSION
        || checkpoint.transcript != transcript.display().to_string()
        || checkpoint.adapter != adapter_name
    {
        return None;
    }
    // Once `modified` is true it is a session-scoped fact that never goes
    // back to `false` (see `session_has_modification`'s own doc comment):
    // the `offset`/transcript-length check below exists only to validate an
    // incremental *resume point*, which a already-`true` checkpoint has no
    // further use for -- requiring it here would mean a transcript that
    // later shrinks, moves, or is cleaned up mid-session could silently
    // forget a modification this session already made.
    if checkpoint.modified {
        return Some(checkpoint);
    }
    let usable = std::fs::metadata(transcript).is_ok_and(|m| checkpoint.offset <= m.len());
    usable.then_some(checkpoint)
}

/// Best-effort, like `save_correction_checkpoint`.
fn save_modification_checkpoint(path: &Path, checkpoint: &ModificationCheckpoint) {
    let Ok(json) = serde_json::to_string(checkpoint) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// Whether `transcript` has shown at least one modification (edit-like) tool
/// call this session -- cheap and incremental like `corrections_in`: only the
/// bytes appended since the last call are parsed, via the same `Watcher`
/// cursor `fold_adoption_delta`/`corrections_in` already use, and
/// `adoption::signals`' own `EDIT_LIKE_TOOLS` list decides what counts (the
/// same signal `AdoptionRecord::edit_like_calls` uses, just folded into its
/// own checkpoint here instead of that one -- see this function's own
/// caller's doc comment for why). Once `modified` is persisted `true`, a
/// later call short-circuits before touching the transcript at all -- a
/// session-scoped fact never goes back to `false`. Originally only ever
/// called once `cfg.verify_on_stop.enabled` is true (see the call site in
/// `verify_on_stop_nudge`), so a session that has that feature off never
/// pays even this bounded parse; `pub(crate)` so `diagnostics::
/// post_edit_nudge` (issue #308) can gate its own, unrelated feature on the
/// identical session-scoped fact rather than re-deriving it.
pub(crate) fn session_has_modification(
    state: &StateDir,
    transcript: &Path,
    cfg: &CtxConfig,
) -> bool {
    let Ok(adapter) = adapters::select_for_identity(cfg.agent.as_deref(), &[], cfg) else {
        return false;
    };
    let path = modification_checkpoint_path(state, transcript);
    let mut checkpoint = load_modification_checkpoint(&path, transcript, adapter.name())
        .unwrap_or_else(|| ModificationCheckpoint {
            version: MODIFICATION_CHECKPOINT_VERSION,
            transcript: transcript.display().to_string(),
            adapter: adapter.name().to_string(),
            modified: false,
            offset: 0,
            consumed: 0,
        });
    if checkpoint.modified {
        return true;
    }
    if !adapter.capabilities().events {
        return false;
    }
    let mut watcher = Watcher::resuming(
        transcript.to_path_buf(),
        checkpoint.offset,
        checkpoint.consumed,
    );
    let Ok(Some(appended)) = watcher.read_appended() else {
        return checkpoint.modified;
    };
    if appended.restarted {
        checkpoint.modified = false;
    }
    if !checkpoint.modified {
        let events = adapter.parse_events(&appended.lines);
        if adoption::signals(&events).edit_like_calls > 0 {
            checkpoint.modified = true;
        }
    }
    let (offset, consumed) = watcher.position();
    checkpoint.offset = offset;
    checkpoint.consumed = consumed;
    save_modification_checkpoint(&path, &checkpoint);
    checkpoint.modified
}

/// Issue #309: whether every entry in `paths` is doc-only -- extension
/// `md`/`txt`/`rst`, or under a root-level `docs/` prefix -- in which case a
/// verify nudge would be noise: neither `zirv test changed` nor `zirv
/// verify` has anything to check in a documentation-only change. Vacuously
/// `true` for an empty slice, the same "nothing to point to" reading
/// `changed_paths` itself gives an untouched worktree.
/// Issue #309: whether `phase` is a step that itself already gates on fresh
/// verification evidence -- `engine::advance`'s own Test/Verify check prints
/// exactly the "run `zirv test changed`/`zirv verify`" message a Stop-hook
/// nudge would otherwise duplicate the moment the operator tries to
/// complete that step.
fn workflow_step_covers_verification(phase: WorkflowPhase) -> bool {
    matches!(phase, WorkflowPhase::Test | WorkflowPhase::Verify)
}

/// The exact command a verify-on-stop nudge names. Reached only once
/// `workflow_step_covers_verification` has already ruled out both Test and
/// Verify for the active step (see `verify_on_stop_nudge`'s own early
/// return), so the `Verify` arm here is presently unreachable through that
/// caller -- kept anyway as the direct mirror of `engine::advance`'s own
/// `if final_only { "zirv verify" } else { "zirv test changed" }` naming, in
/// case a future change narrows the suppression rule to `Test` alone.
/// Bumped whenever `VerifyOnStopRecord`'s own shape changes -- deliberately
/// a separate constant from `MODIFICATION_CHECKPOINT_VERSION` even though
/// both start at `1`: the two checkpoints have unrelated schemas and must be
/// free to version independently.
const VERIFY_ON_STOP_RECORD_VERSION: u32 = 1;

/// Issue #309: how many verify-on-stop nudges this session has already
/// received, one file per session id (mirrors `adoption_record_path`'s own
/// naming/hash scheme).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VerifyOnStopRecord {
    #[serde(default)]
    version: u32,
    nudges: u32,
}

fn verify_on_stop_record_path(state: &StateDir, session: &str) -> PathBuf {
    state
        .scoring()
        .join(format!("{:016x}-verify-on-stop.json", input_hash(session)))
}

/// `Default` (no nudges yet) on any doubt at all, or a different schema
/// version -- like every other hook state read, never a hook failure.
fn load_verify_on_stop_record(path: &Path) -> VerifyOnStopRecord {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<VerifyOnStopRecord>(&body).ok())
        .filter(|record| record.version == VERIFY_ON_STOP_RECORD_VERSION)
        .unwrap_or_default()
}

fn save_verify_on_stop_record(path: &Path, record: &VerifyOnStopRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// Issue #309: Stop-hook advisory naming the exact stale-gate command when
/// code changed this session after the last passing verification run.
///
/// `None` on any doubt at all -- like every other hook advisory, this must
/// never fail loudly. A read-only turn (no modification tool call) runs no
/// git command at all: `session_has_modification` is checked first, and
/// every `verification::*` call below (all of which shell out to git) only
/// runs once that gate is true.
pub(super) fn verify_on_stop_nudge(
    state: &StateDir,
    repo: &Path,
    session: &str,
    cfg: &CtxConfig,
    transcript: &Path,
) -> Option<String> {
    if !cfg.verify_on_stop.enabled {
        return None;
    }
    if !session_has_modification(state, transcript, cfg) {
        return None;
    }
    // Any doubt here (no git, no repo, ...) reads as "fresh": a nudge is
    // advisory, never worth a false positive over an unreadable repo state.
    // No specific workflow branch in view here (a generic advisory nudge),
    // so this never widens to a sibling worktree's evidence -- see
    // `latest_is_fresh_and_passing`'s own doc comment.
    if verification::latest_is_fresh_and_passing(state, repo, false, None).unwrap_or(true) {
        return None;
    }
    let changed = verification::changed_paths(repo).ok()?;
    let active_phase = engine::load_active(state, repo)
        .ok()
        .flatten()
        .and_then(|workflow| workflow.current().map(|step| step.phase));
    // Issue #478: whether fresh evidence is owed, and which command produces
    // it, is the shared verification service's decision -- a native session
    // asks the same question with no transcript and no hook payload.
    let owed = crate::commands::ctx::lifecycle::verification(
        true,
        &changed,
        active_phase.is_some_and(workflow_step_covers_verification),
        active_phase == Some(WorkflowPhase::Verify),
    );
    if !matches!(
        owed,
        crate::commands::ctx::lifecycle::VerificationDecision::Required { .. }
    ) {
        return None;
    }

    let path = verify_on_stop_record_path(state, session);
    let mut record = load_verify_on_stop_record(&path);
    if record.nudges >= cfg.verify_on_stop.max_nudges {
        return None;
    }
    record.version = VERIFY_ON_STOP_RECORD_VERSION;
    record.nudges += 1;
    save_verify_on_stop_record(&path, &record);

    let crate::commands::ctx::lifecycle::VerificationDecision::Required { command } = owed else {
        return None;
    };
    Some(format!(
        "zirv ctx: code changed since the last passing run; run `{command}` before relying on this session's own verification."
    ))
}

#[cfg(test)]
mod tests {
    use super::super::prompt::prompt_adoption_nudge;
    use super::super::tests::{
        correction_heavy_transcript, git_repo, payload, score_with_turns, transcript_with_edits,
    };
    use super::*;
    use crate::commands::ctx::lifecycle::changes_are_doc_only;
    use crate::commands::ctx::rot::{Score, Signals, Verdict};

    fn verify_on_stop_command(active_phase: Option<WorkflowPhase>) -> &'static str {
        crate::commands::ctx::lifecycle::verification_command(
            active_phase == Some(WorkflowPhase::Verify),
        )
    }

    /// Matches `ScoreConfig::default().same_error_threshold` -- every test
    /// below that doesn't care about the same-error clause passes this so a
    /// quiet `same_error_repeats: 0` (`score_of`'s own default) never trips
    /// it by accident.
    const SAME_ERROR_THRESHOLD: usize = 3;

    fn score_of(verdict: Verdict, score: u32) -> Score {
        Score {
            score,
            verdict,
            signals: Signals {
                turns: 12,
                tool_failure_rate: 1.0,
                repetition_hits: 0,
                max_repeat: 1,
                same_error_repeats: 0,
                provider_overflows: 0,
                marker_miss_rate: Some(1.0),
            },
            context_tokens: 170_000,
            model_change: None,
            window_breakdown: None,
        }
    }

    /// Issue #312: a claude transcript with one large `Read` of `/big.rs`
    /// followed by an `Edit` of the same path -- the read's content is live
    /// until the edit stales it. `big_len` controls how many bytes of stale
    /// content this fixture carries, so a test can push it above or below
    /// `compact_advisory.min_reclaim_tokens`'s gate deliberately.
    fn stale_read_then_edit_transcript(big_len: usize, tokens: u64) -> String {
        let big = "x".repeat(big_len);
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"r1\",\"name\":\"Read\",\"input\":{{\"file_path\":\"/big.rs\"}}}}],\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"r1\",\"content\":\"{big}\"}}]}}}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"e1\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"/big.rs\"}}}}],\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"e1\",\"content\":\"ok\"}}]}}}}\n"
        )
    }

    /// Claude's own conservative window for an unstated model
    /// (`ClaudeAdapter::DEFAULT_CONTEXT_WINDOW_TOKENS`), duplicated here as a
    /// plain constant since it is private to `adapters::claude` -- every
    /// test below sizes its `context_tokens` fixture off this number so the
    /// `window_fraction` gate's arithmetic is exact rather than guessed.
    const CLAUDE_DEFAULT_WINDOW: u64 = 200_000;

    #[test]
    fn compact_advisory_fires_when_both_gates_clear() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let transcript_dir = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_dir.path().join("session.jsonl");
        // Window fraction: 150_000 / 200_000 = 0.75, above the default 0.6.
        let tokens = 150_000u64;
        std::fs::write(&transcript, stale_read_then_edit_transcript(50_000, tokens))
            .expect("write transcript");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let score = Score {
            context_tokens: tokens,
            ..score_of(Verdict::Healthy, 0)
        };

        let advisory = compact_advisory_stop_nudge(
            &state,
            repo_dir.path(),
            &cfg,
            &score,
            &transcript,
            &adapter,
        )
        .expect("both gates should clear with 50k stale bytes at 75% of the window");
        assert!(advisory.contains("stale"), "got {advisory}");
        assert!(advisory.contains("/compact"), "got {advisory}");
        assert!(
            advisory.contains("Read"),
            "names the stale source: {advisory}"
        );
    }

    /// Hook start-up overhead fix (wrapper-overhead benchmark, 2026-09-24):
    /// a fresh, unexpired `cached_prompt_bytes` on the checkpoint is used
    /// as-is, never resampled via `compile::compile_with_harness_roster`.
    /// Proven by seeding an absurdly large `system_bytes` before the call:
    /// `BreakdownAccumulator::materialize`'s apportionment gives the
    /// stale-tool-result bucket a share of `total_tokens` proportional to
    /// its OWN byte weight against every other bucket's, so an inflated
    /// `system_bytes` starves that share below `min_reclaim_tokens` even
    /// though the exact same transcript fires the advisory in
    /// `compact_advisory_fires_when_both_gates_clear` above with the real
    /// (small) sampled value -- the only way that can happen is if the
    /// seeded value was used, not a fresh compile.
    #[test]
    fn compact_advisory_reuses_a_fresh_cached_prompt_bytes_sample() {
        use crate::commands::ctx::adapters::AgentAdapter as _;
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let transcript_dir = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_dir.path().join("session.jsonl");
        let tokens = 150_000u64;
        std::fs::write(&transcript, stale_read_then_edit_transcript(50_000, tokens))
            .expect("write transcript");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let score = Score {
            context_tokens: tokens,
            ..score_of(Verdict::Healthy, 0)
        };

        let path = compact_advisory_checkpoint_path(&state, &transcript);
        save_compact_advisory_checkpoint(
            &path,
            &CompactAdvisoryCheckpoint {
                version: COMPACT_ADVISORY_CHECKPOINT_VERSION,
                transcript: transcript.display().to_string(),
                adapter: adapter.name().to_string(),
                accumulator: crate::commands::ctx::breakdown::BreakdownAccumulator::default(),
                offset: 0,
                consumed: 0,
                last_fired_window_tokens: None,
                cached_prompt_bytes: Some(CachedPromptBytes {
                    system_bytes: 100_000_000,
                    schema_bytes: None,
                    sampled_at: now_secs(),
                }),
            },
        );

        assert_eq!(
            compact_advisory_stop_nudge(
                &state,
                repo_dir.path(),
                &cfg,
                &score,
                &transcript,
                &adapter,
            ),
            None,
            "an inflated seeded system_bytes must starve the stale-tool-result share below the \
             reclaim floor, proving the seeded cache -- not a fresh compile -- was used"
        );
    }

    #[test]
    fn compact_advisory_does_not_fire_below_the_reclaim_floor() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let transcript_dir = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_dir.path().join("session.jsonl");
        let tokens = 150_000u64;
        // A single stale byte is not enough to clear `min_reclaim_tokens`
        // even at a generous window fraction.
        std::fs::write(&transcript, stale_read_then_edit_transcript(1, tokens))
            .expect("write transcript");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let score = Score {
            context_tokens: tokens,
            ..score_of(Verdict::Healthy, 0)
        };

        assert_eq!(
            compact_advisory_stop_nudge(
                &state,
                repo_dir.path(),
                &cfg,
                &score,
                &transcript,
                &adapter
            ),
            None,
            "one stale byte must not clear the reclaim floor"
        );
    }

    #[test]
    fn compact_advisory_does_not_fire_below_the_window_fraction() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let transcript_dir = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_dir.path().join("session.jsonl");
        // 10_000 / 200_000 = 5%, well below the default 60% trigger, even
        // though this fixture carries the same 50k stale bytes as the
        // firing test above.
        let tokens = 10_000u64;
        std::fs::write(&transcript, stale_read_then_edit_transcript(50_000, tokens))
            .expect("write transcript");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        let score = Score {
            context_tokens: tokens,
            ..score_of(Verdict::Healthy, 0)
        };

        assert_eq!(
            compact_advisory_stop_nudge(
                &state,
                repo_dir.path(),
                &cfg,
                &score,
                &transcript,
                &adapter
            ),
            None,
            "5% of the window must not clear the window_fraction gate"
        );
    }

    /// Issue #312's own hysteresis acceptance criterion: once the advisory
    /// fires at a given window size, it must not refire until the window has
    /// regrown a full trigger-sized runway (`window_fraction * window`)
    /// PAST that point -- even across several more Stop-hook calls at the
    /// same or a slightly larger window -- and must be allowed to fire again
    /// once it genuinely has.
    #[test]
    fn compact_advisory_rearms_only_after_a_full_trigger_sized_runway() {
        let repo_dir = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let transcript_dir = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_dir.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            stale_read_then_edit_transcript(50_000, 150_000),
        )
        .expect("write transcript");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let cfg = CtxConfig::default();
        // Default `window_fraction` is 0.6; the trigger-sized runway on
        // `CLAUDE_DEFAULT_WINDOW` is 120_000 tokens.
        let trigger_tokens = (0.6 * CLAUDE_DEFAULT_WINDOW as f64) as u64;

        let first_score = Score {
            context_tokens: 150_000,
            ..score_of(Verdict::Healthy, 0)
        };
        let first = compact_advisory_stop_nudge(
            &state,
            repo_dir.path(),
            &cfg,
            &first_score,
            &transcript,
            &adapter,
        );
        assert!(first.is_some(), "the first call at 150_000 must fire");

        // Same window, another Stop-hook call (a fresh process would reload
        // the persisted checkpoint the same way): must not refire.
        let second = compact_advisory_stop_nudge(
            &state,
            repo_dir.path(),
            &cfg,
            &first_score,
            &transcript,
            &adapter,
        );
        assert_eq!(second, None, "an unchanged window must not refire");

        // Grown, but not by a full trigger-sized runway past the point it
        // fired (150_000 + 120_000 = 270_000): still must not refire.
        let partly_regrown_score = Score {
            context_tokens: 150_000 + trigger_tokens - 1,
            ..score_of(Verdict::Healthy, 0)
        };
        let third = compact_advisory_stop_nudge(
            &state,
            repo_dir.path(),
            &cfg,
            &partly_regrown_score,
            &transcript,
            &adapter,
        );
        assert_eq!(
            third, None,
            "one token short of a full trigger-sized runway must not refire"
        );

        // Now a full trigger-sized runway past the firing point: must rearm.
        let fully_regrown_score = Score {
            context_tokens: 150_000 + trigger_tokens,
            ..score_of(Verdict::Healthy, 0)
        };
        let fourth = compact_advisory_stop_nudge(
            &state,
            repo_dir.path(),
            &cfg,
            &fully_regrown_score,
            &transcript,
            &adapter,
        );
        assert!(
            fourth.is_some(),
            "a full trigger-sized runway past the firing point must rearm"
        );
    }

    #[test]
    fn a_healthy_session_prints_nothing() {
        assert_eq!(
            stop_output(
                &payload(),
                &score_of(Verdict::Healthy, 10),
                None,
                None,
                None,
                SAME_ERROR_THRESHOLD,
            ),
            None
        );
    }

    #[test]
    fn an_advisory_verdict_prints_a_non_blocking_system_message() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Advise, 45),
            None,
            None,
            None,
            SAME_ERROR_THRESHOLD,
        )
        .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed["systemMessage"].is_string());
        assert!(
            parsed.get("decision").is_none(),
            "the hook must never block the stop: {out}"
        );
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(text.contains("advise"), "verdict should be named: {text}");
        assert!(
            !text.contains('\u{2014}'),
            "no em dashes in user-facing copy"
        );
        assert!(
            !text.contains("repeated"),
            "no repetition clause when repetition did not drive the verdict: {text}"
        );
        assert!(
            !text.contains("Same error"),
            "no same-error clause when signals are quiet: {text}"
        );
    }

    /// The over-verification clause only fires when `repetition_hits > 0` --
    /// otherwise the advisory reads exactly as it always has (asserted by
    /// `an_advisory_verdict_prints_a_non_blocking_system_message` above).
    #[test]
    fn a_repetition_driven_verdict_gets_the_over_verification_clause() {
        let mut score = score_of(Verdict::Advise, 45);
        score.signals.repetition_hits = 1;
        score.signals.max_repeat = 4;
        let out = stop_output(&payload(), &score, None, None, None, SAME_ERROR_THRESHOLD)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            text.contains("Same tool call repeated 4x with no edit in between"),
            "over-verification clause should be named: {text}"
        );
        assert!(
            !text.contains('\u{2014}'),
            "no em dashes in user-facing copy"
        );
    }

    #[test]
    fn a_restart_verdict_still_only_advises() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Restart, 95),
            None,
            None,
            None,
            SAME_ERROR_THRESHOLD,
        )
        .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(parsed.get("decision").is_none());
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            text.contains("zirv ctx resume"),
            "point at recovery: {text}"
        );
    }

    #[test]
    fn when_a_supervisor_owns_the_session_the_hook_stays_silent() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Restart, 95),
            Some(std::path::Path::new("/tmp/s/ab.sock")),
            None,
            None,
            SAME_ERROR_THRESHOLD,
        );
        assert_eq!(out, None, "the supervisor intervenes, not the hook");
    }

    /// Ported canary case 7: never fire twice in a row.
    #[test]
    fn stop_hook_active_short_circuits_everything() {
        let mut p = payload();
        p.stop_hook_active = true;
        assert_eq!(
            stop_output(
                &p,
                &score_of(Verdict::Restart, 95),
                None,
                None,
                None,
                SAME_ERROR_THRESHOLD,
            ),
            None
        );
    }

    /// Issue #hook-same-error (wrapper proportionality audit follow-through):
    /// `rot::Signals::same_error_repeats` meeting or crossing the operator's
    /// own `same_error_threshold` gets its own clause, distinct from the
    /// repetition clause above -- a stuck same-error loop across different
    /// attempts is not the same failure as an unchanged tool call repeated
    /// with no edit in between.
    #[test]
    fn a_same_error_streak_at_the_threshold_gets_its_own_clause() {
        let mut score = score_of(Verdict::Advise, 45);
        score.signals.same_error_repeats = SAME_ERROR_THRESHOLD;
        let out = stop_output(&payload(), &score, None, None, None, SAME_ERROR_THRESHOLD)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            text.contains(&format!(
                "Same error {SAME_ERROR_THRESHOLD}x in a row across different attempts"
            )),
            "same-error clause should be named at the threshold: {text}"
        );
        assert!(
            !text.contains('\u{2014}'),
            "no em dashes in user-facing copy"
        );
    }

    /// Below the threshold, no clause -- the signal must genuinely cross the
    /// operator's own configured value, not merely be non-zero.
    #[test]
    fn a_same_error_streak_below_the_threshold_gets_no_clause() {
        let mut score = score_of(Verdict::Advise, 45);
        score.signals.same_error_repeats = SAME_ERROR_THRESHOLD - 1;
        let out = stop_output(&payload(), &score, None, None, None, SAME_ERROR_THRESHOLD)
            .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            !text.contains("Same error"),
            "no same-error clause below the threshold: {text}"
        );
    }

    /// Review finding F2: a threshold of 0 is how an operator disables the
    /// same-error signal outright. Without the `> 0` guard, `repeats >= 0`
    /// is trivially true and the clause fires on every non-healthy advisory
    /// even though the operator asked for it to never fire.
    #[test]
    fn a_zero_threshold_disables_the_same_error_clause_even_with_repeats() {
        let mut score = score_of(Verdict::Advise, 45);
        score.signals.same_error_repeats = 5;
        let out = stop_output(&payload(), &score, None, None, None, 0).expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            !text.contains("Same error"),
            "threshold 0 must disable the clause even with repeats: {text}"
        );
    }

    /// Issue #223: `adoption_stop_nudge` is `off` -- no record is even
    /// written, since nothing about it may ever be consulted.
    #[test]
    fn adoption_off_writes_no_record_and_nudges_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Off;

        let text = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-off",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        assert_eq!(text, None);
        assert!(
            !adoption_record_path(&state, "sess-off").exists(),
            "off must not even persist a record"
        );
    }

    /// A delegated worker (carrying `agent::WORK_GROUP_ENV`) is never
    /// nudged, no matter how substantial its own work looks.
    #[test]
    fn adoption_skips_a_delegated_worker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Nudge;
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::agent::WORK_GROUP_ENV.to_string(),
            "wg-1".to_string(),
        )]
        .into();

        let text = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-worker",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|k| env.get(k).cloned(),
        );
        assert_eq!(text, None, "a delegated worker must never be nudged");
    }

    /// Substantial work (>= 12 edit calls) with no active workflow, under
    /// `nudge`: the first call nudges immediately and persists a record
    /// saying so; an unchanged follow-up call (no new turns) stays silent.
    #[test]
    fn adoption_nudges_once_immediately_then_holds_until_the_next_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Nudge;

        let first = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-nudge",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        )
        .expect("substantial work must nudge immediately");
        assert!(first.contains("12 edit calls over 12 turns"), "{first}");
        assert!(first.contains("zirv workflow start"), "{first}");

        // Same transcript, same turn count -- nothing new happened, so the
        // cooldown must hold.
        let second = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-nudge",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        assert_eq!(second, None, "must not nudge twice for the same turn");
    }

    /// `advise` fires exactly once -- `nudge_due` itself never fires below
    /// `Nudge`, so the Stop hook's own one-shot path is what must produce the
    /// single notice here.
    #[test]
    fn adoption_advise_nudges_exactly_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Advise;

        let first = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-advise",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        assert!(first.is_some(), "advise must still fire once");

        let second = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-advise",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        assert_eq!(second, None, "advise must never repeat");
    }

    /// `enforce` carries the same nudge text plus the delegation-gate
    /// sentence.
    #[test]
    fn adoption_enforce_appends_the_delegation_gate_sentence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Enforce;

        let text = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-enforce",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        )
        .expect("enforce must still nudge");
        assert!(text.contains("workflow.adoption = enforce"), "{text}");
        assert!(text.contains("zirv agent delegation is held"), "{text}");
    }

    /// A session with an active workflow is never nudged even though its own
    /// edit-call count would otherwise be substantial -- and the telemetry
    /// recorded for it says the workflow was already active at detection.
    #[test]
    fn adoption_stays_silent_and_records_workflow_active_at_detection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        crate::commands::workflow::engine::save(
            &state,
            &crate::commands::workflow::engine::WorkflowState::start(
                repo.path().to_path_buf(),
                "task".into(),
                crate::commands::workflow::engine::WorkflowKind::Feature,
                None,
                true,
                classify::classify(&classify::ClassificationInput {
                    task: String::new(),
                    paths: Vec::new(),
                    changed_lines: 0,
                    tests_changed: true,
                    intent_override: None,
                    complexity_override: None,
                    risk_override: None,
                })
                .expect("classify"),
            ),
            true,
        )
        .expect("save active workflow");
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Nudge;
        // This test is about the WORKFLOW nudge's own active-workflow
        // suppression, not the skill nudge: that one fires independent of
        // `workflow_active` by design (skills matter inside a workflow too),
        // so it would otherwise also produce text here (zero skill loads,
        // substantial work) and make this assertion meaningless. Disabling it
        // keeps the assertion about what this test actually covers.
        cfg.prompt.skill_index = false;

        let text = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-active",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        assert_eq!(text, None, "an active workflow must never be nudged");

        let events = telemetry::list(&state, repo.path()).expect("list");
        let detected = events
            .iter()
            .find(|e| e.kind == telemetry::TelemetryKind::AdoptionDetected)
            .expect("AdoptionDetected must still be recorded");
        assert_eq!(detected.workflow_active, Some(true));
    }

    /// Once a session is recorded as substantial with no active workflow, a
    /// later call that finds one active records exactly one
    /// `AdoptionRecovered` event.
    #[test]
    fn adoption_records_recovery_once_a_workflow_starts_after_detection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Nudge;

        adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-recover",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        let events = telemetry::list(&state, repo.path()).expect("list");
        let detected = events
            .iter()
            .find(|e| e.kind == telemetry::TelemetryKind::AdoptionDetected)
            .expect("must record detection");
        assert_eq!(detected.workflow_active, Some(false));

        crate::commands::workflow::engine::save(
            &state,
            &crate::commands::workflow::engine::WorkflowState::start(
                repo.path().to_path_buf(),
                "task".into(),
                crate::commands::workflow::engine::WorkflowKind::Feature,
                None,
                true,
                classify::classify(&classify::ClassificationInput {
                    task: String::new(),
                    paths: Vec::new(),
                    changed_lines: 0,
                    tests_changed: true,
                    intent_override: None,
                    complexity_override: None,
                    risk_override: None,
                })
                .expect("classify"),
            ),
            true,
        )
        .expect("save active workflow");

        adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-recover",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        );
        let events = telemetry::list(&state, repo.path()).expect("list");
        let recovered = events
            .iter()
            .filter(|e| e.kind == telemetry::TelemetryKind::AdoptionRecovered)
            .count();
        assert_eq!(recovered, 1, "recovery must be recorded exactly once");
    }

    /// Appends `turns` more claude-shaped turns to a transcript
    /// `transcript_with_edits` already built, no edit-like tool calls in any
    /// of them, and a native `skill_load` tool_use in the LAST one when
    /// `with_skill_load` is true. Fixture for proving `fold_adoption_delta`
    /// really counts a skill-load tool call from a real transcript fold, not
    /// just the pure `adoption::signals` unit test.
    fn append_turns(path: &std::path::Path, turns: usize, with_skill_load: bool) {
        let mut text = std::fs::read_to_string(path).expect("read");
        for i in 0..turns {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            let mut content = "{\"type\":\"text\",\"text\":\"ok\"}".to_string();
            if with_skill_load && i + 1 == turns {
                content.push_str(
                    ",{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"skill_load\",\"input\":{}}",
                );
            }
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{content}],\"usage\":{{\"input_tokens\":100}}}}}}\n"
            ));
        }
        std::fs::write(path, text).expect("write");
    }

    /// Substantial work with zero skill loads fires the skill nudge; once a
    /// native/MCP `skill_load` tool call actually appears in the transcript,
    /// the same cadence that would otherwise still be due (turn 18 >= the
    /// first fire's turn 12, plus `NUDGE_EVERY_TURNS`) stays silent instead
    /// -- proving `record.skill_loads` is really folded from the transcript,
    /// not merely
    /// checked in the pure unit.
    #[test]
    fn adoption_stop_nudge_skill_nudge_fires_then_falls_silent_once_a_load_is_folded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let cfg = CtxConfig::default();
        assert_eq!(cfg.workflow.adoption, AdoptionPolicy::Nudge);

        let first = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-skill-nudge",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        )
        .expect("substantial with zero loads must nudge");
        assert!(
            first.contains(
                "[zirv skills] substantial work (12 edit calls over 12 turns) and no zirv \
                 skill loaded this session"
            ),
            "{first}"
        );

        // Six more turns, no new edits, a `skill_load` tool call in the last
        // one -- cadence alone (turn 18 >= 12 + 5) would still say due.
        append_turns(&transcript, 6, true);
        let second = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-skill-nudge",
            &cfg,
            &score_with_turns(18),
            &transcript,
            &|_| None,
        )
        .unwrap_or_default();
        assert!(
            !second.contains("[zirv skills]"),
            "a folded skill load must silence the skill nudge: {second}"
        );
    }

    /// `prompt.skill_index = false` turns the skill nudge off entirely,
    /// independent of everything else about the session being substantial
    /// with zero loads.
    #[test]
    fn adoption_stop_nudge_skill_nudge_is_off_when_skill_index_is_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let mut cfg = CtxConfig::default();
        cfg.prompt.skill_index = false;

        let text = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-skill-off",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        )
        .unwrap_or_default();
        assert!(
            !text.contains("[zirv skills]"),
            "skill_index=false must turn the skill nudge off: {text}"
        );
    }

    /// `shell_skill_loads` (bumped only by `record_shell_skill_load`, tested
    /// at `skill.rs`'s own seam) sums with the transcript-derived `skill_loads`
    /// for the skill nudge's own "zero loads" check -- a shell-recorded load
    /// silences the nudge exactly like a native/MCP tool-call load does.
    #[test]
    fn adoption_stop_nudge_skill_nudge_is_silent_after_a_shell_recorded_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 12, 12);
        let cfg = CtxConfig::default();

        // Simulate `zirv skill load` already having run once from this same
        // session's own shell, exactly as `record_shell_skill_load` does.
        let path = adoption_record_path(&state, "sess-shell-nudge");
        save_adoption_record(
            &path,
            &AdoptionRecord {
                shell_skill_loads: 1,
                ..Default::default()
            },
        );

        let text = adoption_stop_nudge(
            &state,
            repo.path(),
            "sess-shell-nudge",
            &cfg,
            &score_with_turns(12),
            &transcript,
            &|_| None,
        )
        .unwrap_or_default();
        assert!(
            !text.contains("[zirv skills]"),
            "a shell-recorded load must silence the skill nudge: {text}"
        );
    }

    /// `shell_skill_loads` is not transcript-derived at all, so a restarted
    /// transcript -- which resets `edit_like_calls`/`skill_loads` to zero --
    /// must leave it untouched.
    #[test]
    fn fold_adoption_delta_never_touches_shell_skill_loads_even_on_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = transcript_with_edits(dir.path(), 3, 3);
        let mut record = AdoptionRecord {
            shell_skill_loads: 4,
            ..Default::default()
        };
        let cfg = CtxConfig::default();
        let adapter =
            adapters::select_for_identity(cfg.agent.as_deref(), &[], &cfg).expect("adapter");

        fold_adoption_delta(&mut record, &transcript, adapter.as_ref());
        assert_eq!(record.shell_skill_loads, 4, "first fold must not touch it");
        assert_eq!(record.edit_like_calls, 3);

        // A shorter file at the same path is exactly what `Watcher::
        // read_appended` reads as a restart (`len < self.offset`).
        let restarted = transcript_with_edits(dir.path(), 1, 1);
        fold_adoption_delta(&mut record, &restarted, adapter.as_ref());
        assert_eq!(record.edit_like_calls, 1, "a restart does reset this one");
        assert_eq!(
            record.shell_skill_loads, 4,
            "a transcript restart must never reset the shell-load counter"
        );
    }

    /// Issue #293: `record_speed_sample` writes exactly one
    /// `TurnLatencySampled` event, carrying the sample's four fields and
    /// this call's `session`.
    #[test]
    fn record_speed_sample_writes_one_turn_latency_sampled_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let cfg = CtxConfig::default();

        let speed = crate::commands::ctx::event::SpeedMetrics {
            turn_p50_ms: Some(400),
            turn_max_ms: Some(900),
            ttft_p50_ms: Some(150),
            tool_error_rate: Some(0.2),
        };

        record_speed_sample(&state, repo.path(), "sess-speed", &cfg, Some(speed));

        let events = telemetry::list(&state, repo.path()).expect("list");
        let sampled = events
            .iter()
            .find(|e| e.kind == telemetry::TelemetryKind::TurnLatencySampled)
            .expect("TurnLatencySampled must be recorded");
        assert_eq!(sampled.session_id, Some("sess-speed".to_string()));
        assert_eq!(sampled.turn_p50_ms, Some(400));
        assert_eq!(sampled.turn_max_ms, Some(900));
        assert_eq!(sampled.ttft_p50_ms, Some(150));
        assert_eq!(sampled.tool_error_rate, Some(0.2));
    }

    /// No speed data (`None`, the ordinary case for a poll whose appended
    /// events carried no usable timestamps) records nothing -- never a
    /// fabricated all-`None` sample.
    #[test]
    fn record_speed_sample_is_a_no_op_when_speed_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let cfg = CtxConfig::default();

        record_speed_sample(&state, repo.path(), "sess-nospeed", &cfg, None);

        let events = telemetry::list(&state, repo.path()).expect("list");
        assert!(
            !events
                .iter()
                .any(|e| e.kind == telemetry::TelemetryKind::TurnLatencySampled),
            "no speed data means nothing to record"
        );
    }

    /// The Prompt hook's own live re-check: a record still saying
    /// substantial-without-workflow must not fire once a workflow has
    /// actually started, even though the record on disk has not caught up
    /// yet (only the next Stop call refreshes it).
    #[test]
    fn prompt_adoption_nudge_live_check_suppresses_once_a_workflow_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let session = "sess-prompt-1";
        let path = adoption_record_path(&state, session);
        save_adoption_record(&path, &AdoptionRecord::substantial_for_test(7, 9));

        let mut cfg = CtxConfig::default();
        cfg.workflow.adoption = AdoptionPolicy::Nudge;
        let env: std::collections::HashMap<String, String> =
            [(SESSION_ENV.to_string(), session.to_string())].into();
        let state_env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            dir.path().display().to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned().or_else(|| state_env.get(k).cloned());

        // Before any workflow exists, the live re-check finds none, so it
        // nudges exactly like the record on disk suggests.
        let before = prompt_adoption_nudge(repo.path(), &cfg, &lookup);
        assert!(before.is_some(), "no active workflow yet: must nudge");

        crate::commands::workflow::engine::save(
            &state,
            &crate::commands::workflow::engine::WorkflowState::start(
                repo.path().to_path_buf(),
                "task".into(),
                crate::commands::workflow::engine::WorkflowKind::Feature,
                None,
                true,
                classify::classify(&classify::ClassificationInput {
                    task: String::new(),
                    paths: Vec::new(),
                    changed_lines: 0,
                    tests_changed: true,
                    intent_override: None,
                    complexity_override: None,
                    risk_override: None,
                })
                .expect("classify"),
            ),
            true,
        )
        .expect("save active workflow");

        // The persisted record still says substantial-without-workflow (only
        // a Stop call would refresh it), but the live re-check must still
        // suppress the nudge.
        let after = prompt_adoption_nudge(repo.path(), &cfg, &lookup);
        assert_eq!(
            after, None,
            "a workflow started in another pane must suppress the nudge"
        );
    }

    /// The skill nudge rides `prompt_adoption_nudge` exactly like it rides
    /// `adoption_stop_nudge` -- fires once substantial with zero loads, silent
    /// once the persisted record already shows one. The record is re-seeded
    /// fresh (cadence fields cleared) between the two reads so this is purely
    /// about `skill_loads`, not cadence.
    #[test]
    fn prompt_adoption_nudge_fires_the_skill_nudge_and_falls_silent_once_a_load_is_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let session = "sess-prompt-skill";
        let path = adoption_record_path(&state, session);
        save_adoption_record(
            &path,
            &AdoptionRecord {
                substantial: true,
                edit_like_calls: 7,
                turns: 9,
                ..Default::default()
            },
        );
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(SESSION_ENV.to_string(), session.to_string())].into();
        let state_env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            dir.path().display().to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned().or_else(|| state_env.get(k).cloned());

        let with_zero_loads = prompt_adoption_nudge(repo.path(), &cfg, &lookup)
            .expect("substantial with zero loads must nudge");
        assert!(
            with_zero_loads.contains("[zirv skills]"),
            "{with_zero_loads}"
        );

        // A load already recorded (as if a Stop call folded one in between)
        // must silence it.
        save_adoption_record(
            &path,
            &AdoptionRecord {
                substantial: true,
                edit_like_calls: 7,
                turns: 9,
                skill_loads: 1,
                ..Default::default()
            },
        );
        let with_a_load = prompt_adoption_nudge(repo.path(), &cfg, &lookup).unwrap_or_default();
        assert!(
            !with_a_load.contains("[zirv skills]"),
            "a recorded load must silence the skill nudge: {with_a_load}"
        );
    }

    /// `stop_output` folds an adoption nudge into a healthy session's
    /// systemMessage even when there is no optimize hint at all.
    #[test]
    fn stop_output_includes_the_adoption_nudge_on_an_otherwise_healthy_session() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Healthy, 10),
            None,
            None,
            Some("[zirv workflow] substantial work detected"),
            SAME_ERROR_THRESHOLD,
        )
        .expect("a healthy session with a due nudge must still print");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(text.contains("substantial work detected"), "{text}");
    }

    /// `stop_output` appends the nudge as its own line after the ordinary
    /// advisory, rather than replacing it.
    #[test]
    fn stop_output_appends_the_adoption_nudge_after_the_advisory() {
        let out = stop_output(
            &payload(),
            &score_of(Verdict::Advise, 45),
            None,
            None,
            Some("[zirv workflow] substantial work detected"),
            SAME_ERROR_THRESHOLD,
        )
        .expect("advisory expected");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let text = parsed["systemMessage"].as_str().unwrap_or_default();
        assert!(
            text.contains("advise"),
            "the rot advisory must survive: {text}"
        );
        assert!(
            text.contains("substantial work detected"),
            "the nudge must be appended: {text}"
        );
    }

    /// Item 1: `corrections_in` must use whichever adapter `cfg` selects
    /// (mirroring `score_transcript`), not a hardcoded claude call.
    #[test]
    fn corrections_are_computed_through_the_configured_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());
        let state = StateDir::from_root(dir.path().join("state"));
        let cfg = CtxConfig::default();
        assert_eq!(corrections_in(&state, &transcript, &cfg), 5);
    }

    /// An adapter with no event parsing wired up (codex today -- out of
    /// scope, see issue #11) must degrade to zero corrections rather than
    /// panic: the recommendation is advisory, never load-bearing. Codex is
    /// selectable now (`CodexAdapter::ready` mirrors claude's), so this
    /// exercises `structural_context`'s all-empty stub rather than a
    /// selection failure, but the degrade-to-zero guarantee is the same one.
    #[test]
    fn corrections_are_zero_for_an_adapter_with_no_event_parsing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());
        let state = StateDir::from_root(dir.path().join("state"));
        let cfg = CtxConfig {
            agent: Some("codex".to_string()),
            ..CtxConfig::default()
        };
        assert_eq!(
            corrections_in(&state, &transcript, &cfg),
            0,
            "an adapter with no parsing degrades to zero corrections, not a panic"
        );
    }

    /// Task A6: `select`'s gate check degrades the same way an adapter with
    /// no event parsing already does -- `corrections_in`'s `Ok(adapter)`
    /// else-branch covers a refused `select`. G (2026-08-15): disabling
    /// claude via a repo-only `.settings.toml` used to fall through to codex
    /// (enabled, and its own `ready()` succeeds) here, exercising `count_
    /// corrections`'s `structural_context` path instead. `resolve_default`
    /// now refuses that silent provider switch outright
    /// (`AgentGate::disabled_only_by_repo`), so this test exercises the
    /// refused-`select` path once more -- the assertion is unchanged (both
    /// paths degrade to zero, not a panic), but for a different reason than
    /// when this test was written.
    #[test]
    fn a_disabled_agent_leaves_the_stop_hook_a_silent_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = correction_heavy_transcript(dir.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        let state = StateDir::from_root(dir.path().join("state"));

        assert_eq!(
            corrections_in(&state, &transcript, &cfg),
            0,
            "a refused fallback (repo may narrow, not select) still degrades to zero, not a panic"
        );
    }

    /// Guards the O(n^2) `corrections_in` fix: once a checkpoint exists for a
    /// transcript, a later call must fold only the bytes appended since then,
    /// never re-read the whole file. Proven by corrupting the entire
    /// already-consumed region with non-UTF-8 garbage -- well clear of the
    /// small head/tail window `Watcher`'s own restart fingerprint samples, so
    /// the checkpoint stays valid -- which `std::fs::read_to_string` (the old,
    /// every-call whole-file implementation) chokes on outright. Only an
    /// implementation that truly never rereads the corrupted bytes can still
    /// find the second correction appended after them.
    #[test]
    fn a_second_call_after_a_checkpoint_never_rereads_the_consumed_region() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("t.jsonl");
        let state = StateDir::from_root(dir.path().join("state"));
        let cfg = CtxConfig::default();

        // One correction, then enough filler lines that the consumed region
        // comfortably clears the watcher's head+tail fingerprint window (4096
        // + 256 bytes) with plenty of room in the middle to corrupt.
        let mut text = String::new();
        text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"no, not like that\"}}\n");
        for _ in 0..400 {
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n",
                "filler line padding out the transcript ".repeat(2)
            ));
        }
        std::fs::write(&transcript, &text).expect("write");

        assert_eq!(
            corrections_in(&state, &transcript, &cfg),
            1,
            "first pass finds the one correction"
        );

        let consumed_len = text.len();
        assert!(
            consumed_len > 8000,
            "the consumed region must comfortably clear the watcher's head+tail fingerprint window, got {consumed_len}"
        );

        // Corrupt the middle of the already-consumed region -- well past the
        // first 4096 bytes and well before the last 256 -- with invalid
        // UTF-8. A full `read_to_string` re-parse of the whole file would
        // hard error on this; the incremental fold must never touch it again.
        let mut bytes = std::fs::read(&transcript).expect("read bytes");
        let corrupt_start = 5000;
        let corrupt_end = bytes.len() - 1000;
        for b in &mut bytes[corrupt_start..corrupt_end] {
            *b = 0xFF;
        }
        // Append a second correction after the corrupted region, growing the
        // file so the watcher reads this as an append, not a same-length
        // rewrite.
        bytes.extend_from_slice(
            b"{\"type\":\"user\",\"message\":{\"content\":\"no, still wrong\"}}\n",
        );
        std::fs::write(&transcript, &bytes).expect("write corrupted+appended");

        // Sanity: the old whole-file approach really would choke on this, or
        // this test proves nothing.
        assert!(
            std::fs::read_to_string(&transcript).is_err(),
            "the corrupted region must actually be invalid UTF-8"
        );

        assert_eq!(
            corrections_in(&state, &transcript, &cfg),
            2,
            "the second correction must still be found even though the whole file is now unreadable as a string -- proof the consumed region was never reread"
        );
    }

    /// Direct test of the changed line (review finding 1, hook.rs half): a
    /// malformed repo `.settings.toml` must not make `cfg_or_operator_only_
    /// gate` fall back to a fully permissive gate. Unlike the end-to-end
    /// test above, this reaches the fallback arm directly, independent of
    /// whatever else in `run_stop` might also happen to fail closed first.
    #[test]
    fn cfg_or_operator_only_gate_denies_what_the_operator_denied_even_with_a_broken_repo_layer() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(repo.join(".zirv/.settings.toml"), "not [ valid toml").expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        assert!(
            CtxConfig::load(&repo, &|k| empty.get(k).cloned()).is_err(),
            "the malformed repo file must actually make CtxConfig::load fail, or this test \
             proves nothing"
        );

        let cfg = cfg_or_operator_only_gate(&repo, &|k| empty.get(k).cloned());
        assert!(
            !cfg.agents.is_enabled("claude"),
            "the operator's disable must survive a repo layer that could not be read"
        );
        assert_eq!(
            cfg.policy,
            crate::commands::ctx::policy::EffectivePolicy::fail_closed(),
            "issue #44: a failed config load must fail closed on policy too, not default to Allow"
        );
    }

    /// A malformed repo policy must fail closed while preserving the
    /// operator's own narrowing through `degrade_to_operator_only`.
    #[test]
    fn a_malformed_repo_policy_table_preserves_the_operators_policy_narrowing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"deny\"\n",
        )
        .expect("write");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".zirv")).expect("mkdir repo");
        std::fs::write(
            repo.join(".zirv/ctx.toml"),
            "[policy]\nshell_exec = \"nope\"\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        assert!(
            CtxConfig::load(&repo, &|k| empty.get(k).cloned()).is_err(),
            "the malformed repo [policy] table must actually make CtxConfig::load fail, or this \
             test proves nothing"
        );

        let cfg = cfg_or_operator_only_gate(&repo, &|k| empty.get(k).cloned());
        assert_eq!(
            cfg.policy.shell_exec,
            crate::commands::ctx::policy::Stance::Deny,
            "the operator's shell_exec=deny survives a broken repo layer"
        );
    }

    #[test]
    fn session_has_modification_is_false_without_an_edit_like_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 3, 0);
        let cfg = CtxConfig::default();
        assert!(!session_has_modification(&state, &transcript, &cfg));
    }

    #[test]
    fn session_has_modification_is_true_with_an_edit_like_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 3, 1);
        let cfg = CtxConfig::default();
        assert!(session_has_modification(&state, &transcript, &cfg));
    }

    /// Once `modified` is persisted `true`, a later call must not need to
    /// re-read the transcript at all: deleting it must not flip the answer
    /// back to `false`.
    #[test]
    fn session_has_modification_short_circuits_once_persisted_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let transcript = transcript_with_edits(dir.path(), 3, 1);
        let cfg = CtxConfig::default();
        assert!(session_has_modification(&state, &transcript, &cfg));
        std::fs::remove_file(&transcript).expect("remove transcript");
        assert!(
            session_has_modification(&state, &transcript, &cfg),
            "a persisted true must never require re-reading the transcript"
        );
    }

    #[test]
    fn changes_are_doc_only_accepts_markdown_txt_rst_and_a_docs_prefix() {
        assert!(changes_are_doc_only(&[]));
        assert!(changes_are_doc_only(&[PathBuf::from("README.md")]));
        assert!(changes_are_doc_only(&[PathBuf::from("notes.txt")]));
        assert!(changes_are_doc_only(&[PathBuf::from("CHANGELOG.rst")]));
        assert!(changes_are_doc_only(&[PathBuf::from("docs/guide.html")]));
        assert!(!changes_are_doc_only(&[PathBuf::from("src/main.rs")]));
        assert!(!changes_are_doc_only(&[
            PathBuf::from("README.md"),
            PathBuf::from("src/main.rs"),
        ]));
    }

    #[test]
    fn workflow_step_covers_verification_matches_test_and_verify_phases_only() {
        assert!(!workflow_step_covers_verification(WorkflowPhase::Implement));
        assert!(!workflow_step_covers_verification(WorkflowPhase::Review));
        assert!(workflow_step_covers_verification(WorkflowPhase::Test));
        assert!(workflow_step_covers_verification(WorkflowPhase::Verify));
    }

    /// Reachable only in theory (see the function's own doc comment): proves
    /// the naming rule directly regardless of the current suppression
    /// choice in `verify_on_stop_nudge`.
    #[test]
    fn verify_on_stop_command_names_zirv_verify_only_for_the_verify_phase() {
        assert_eq!(verify_on_stop_command(None), "zirv test changed");
        assert_eq!(
            verify_on_stop_command(Some(WorkflowPhase::Implement)),
            "zirv test changed"
        );
        assert_eq!(
            verify_on_stop_command(Some(WorkflowPhase::Test)),
            "zirv test changed"
        );
        assert_eq!(
            verify_on_stop_command(Some(WorkflowPhase::Verify)),
            "zirv verify"
        );
    }

    fn feature_classification() -> classify::Classification {
        classify::classify(&classify::ClassificationInput {
            task: String::new(),
            paths: Vec::new(),
            changed_lines: 0,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classify")
    }

    #[test]
    fn verify_on_stop_nudge_fires_for_a_stale_code_change_with_no_active_workflow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let cfg = CtxConfig::default();

        let text = verify_on_stop_nudge(&state, repo.path(), "sess-a", &cfg, &transcript)
            .expect("a stale code change with a real modification must nudge");
        assert!(text.contains("zirv test changed"), "{text}");
    }

    #[test]
    fn verify_on_stop_nudge_is_silent_when_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let mut cfg = CtxConfig::default();
        cfg.verify_on_stop.enabled = false;

        assert_eq!(
            verify_on_stop_nudge(&state, repo.path(), "sess-b", &cfg, &transcript),
            None
        );
    }

    /// Even with a real stale change sitting in the worktree, a transcript
    /// with no modification tool call must never nudge -- proving the
    /// modification gate runs, and runs first.
    #[test]
    fn verify_on_stop_nudge_is_silent_without_a_modification_tool_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 0);
        let cfg = CtxConfig::default();

        assert_eq!(
            verify_on_stop_nudge(&state, repo.path(), "sess-c", &cfg, &transcript),
            None,
            "no modification tool call this session must never nudge"
        );
    }

    #[test]
    fn verify_on_stop_nudge_is_silent_for_a_doc_only_change_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("README.md"), "docs\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let cfg = CtxConfig::default();

        assert_eq!(
            verify_on_stop_nudge(&state, repo.path(), "sess-d", &cfg, &transcript),
            None,
            "a doc-only change set has nothing for a test/verify gate to check"
        );
    }

    /// Issue #478 review finding 5: a session whose transcript shows edit
    /// tool calls but whose working tree is CLEAN owes no fresh evidence --
    /// `changes_are_doc_only(&[])` is vacuously true, and always has been.
    /// The gate this pins is the one the extraction of `verification` into
    /// `lifecycle.rs` could have quietly inverted.
    #[test]
    fn verify_on_stop_nudge_is_silent_when_nothing_actually_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let cfg = CtxConfig::default();

        assert!(
            verification::changed_paths(repo.path())
                .expect("changed paths")
                .is_empty(),
            "this fixture repository must have nothing to verify"
        );
        assert_eq!(
            verify_on_stop_nudge(&state, repo.path(), "sess-empty", &cfg, &transcript),
            None,
            "an empty change set has nothing for a test/verify gate to check"
        );
    }

    #[test]
    fn verify_on_stop_nudge_is_silent_when_the_active_workflow_step_already_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let cfg = CtxConfig::default();

        let mut wf = crate::commands::workflow::engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "task".into(),
            crate::commands::workflow::engine::WorkflowKind::Feature,
            None,
            true,
            feature_classification(),
        );
        let verify_index = wf
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .expect("a Feature workflow has a Verify step");
        wf.current_step = verify_index;
        crate::commands::workflow::engine::save(&state, &wf, true).expect("save active workflow");

        assert_eq!(
            verify_on_stop_nudge(&state, repo.path(), "sess-e", &cfg, &transcript),
            None,
            "the workflow's own Verify-step gate already covers this"
        );
    }

    /// A workflow active on a phase that does not itself gate on
    /// verification (its default first step) must not suppress the nudge.
    #[test]
    fn verify_on_stop_nudge_still_fires_when_the_active_step_is_not_test_or_verify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let cfg = CtxConfig::default();

        let wf = crate::commands::workflow::engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "task".into(),
            crate::commands::workflow::engine::WorkflowKind::Feature,
            None,
            true,
            feature_classification(),
        );
        assert!(
            !workflow_step_covers_verification(wf.current().expect("first step").phase),
            "test setup: the first step must not already be Test/Verify"
        );
        crate::commands::workflow::engine::save(&state, &wf, true).expect("save active workflow");

        assert!(
            verify_on_stop_nudge(&state, repo.path(), "sess-f", &cfg, &transcript).is_some(),
            "a workflow active on an unrelated phase must not suppress the nudge"
        );
    }

    #[test]
    fn verify_on_stop_nudge_caps_at_max_nudges_per_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let transcript = transcript_with_edits(dir.path(), 1, 1);
        let cfg = CtxConfig::default(); // max_nudges: 2

        assert!(
            verify_on_stop_nudge(&state, repo.path(), "sess-g", &cfg, &transcript).is_some(),
            "1st stale turn must nudge"
        );
        assert!(
            verify_on_stop_nudge(&state, repo.path(), "sess-g", &cfg, &transcript).is_some(),
            "2nd stale turn must nudge"
        );
        assert_eq!(
            verify_on_stop_nudge(&state, repo.path(), "sess-g", &cfg, &transcript),
            None,
            "3rd stale turn in the same session must not nudge again"
        );
    }
}
