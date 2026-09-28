use super::*;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    pub window: usize,
    pub min_turns: usize,
    /// Explicit absolute override for the token-pressure floor. Wins outright
    /// over `token_floor_ratio` when set -- an operator who pins a number
    /// gets that number, capacity or not. `None` (the default) means "derive
    /// it from the ratio and the resolved capacity instead"; see
    /// `rot::token_gates`.
    pub token_floor: Option<u64>,
    /// Same as `token_floor`, for the ceiling.
    pub token_ceiling: Option<u64>,
    /// Fraction of the resolved capacity the floor sits at when no explicit
    /// `token_floor` is set (issue #155, Phase 6b). Default `0.5`.
    pub token_floor_ratio: f64,
    /// Fraction of the resolved capacity the ceiling sits at when no
    /// explicit `token_ceiling` is set. Default `0.8`.
    pub token_ceiling_ratio: f64,
    /// Operator-pinned context-window capacity, overriding whatever the
    /// adapter itself reports (`Capabilities::context_window_tokens`): the
    /// operator knows their own seat, and the adapter's default is a guess
    /// about it. `None` (the default) defers to the adapter.
    pub model_context_tokens: Option<u64>,
    pub weight_tool_failure: f64,
    pub weight_repetition: f64,
    pub weight_marker: f64,
    /// Score weight for a stuck same-error loop -- the longest run of
    /// consecutive identical (normalized) tool-result error texts within
    /// the window (`rot::Signals::same_error_repeats`).
    ///
    /// Issue #763: default `120.0`, enabling the signal that used to ship
    /// inert (`0.0`). Chosen, not measured, so that a FRESHLY-tripped streak
    /// -- exactly `same_error_threshold` (default `3`) consecutive identical
    /// errors, `rot::repetition_component`'s own ramp at its lowest nonzero
    /// point, `1 / same_error_threshold` -- raises the score to exactly
    /// `advise_at`'s default (`120.0 * (1.0 / 3.0) == 40.0`) in an otherwise
    /// healthy session: the FIRST action this signal can ever cause is
    /// `advise`, never `compact`/`restart`, matching `DEFAULT_PROMPT`'s own
    /// "stuck twice on the same error: change approach" bullet. A session
    /// that keeps repeating past that point escalates the same way every
    /// other signal does, through the identical weighted-sum/threshold
    /// machinery -- see `rot::score_from`/`verdict_for`. Set `0.0` to restore
    /// the old, fully inert behaviour.
    pub same_error_weight: f64,
    pub repetition_threshold: usize,
    /// Repeat count of the SAME normalized error text before the
    /// same-error signal trips, ramped the same way `repetition_threshold`
    /// ramps `weight_repetition` (via `rot::repetition_component`). Default
    /// `3`.
    pub same_error_threshold: usize,
    pub advise_at: u32,
    pub compact_at: u32,
    pub restart_at: u32,
    pub marker: String,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            window: 10,
            min_turns: 10,
            token_floor: None,
            token_ceiling: None,
            token_floor_ratio: 0.5,
            token_ceiling_ratio: 0.8,
            model_context_tokens: None,
            weight_tool_failure: 40.0,
            weight_repetition: 30.0,
            weight_marker: 30.0,
            same_error_weight: 120.0,
            repetition_threshold: 3,
            same_error_threshold: 3,
            advise_at: 40,
            compact_at: 60,
            restart_at: 80,
            marker: DEFAULT_MARKER.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WrapConfig {
    pub debounce_ms: u64,
    pub inject_timeout_ms: u64,
}

impl Default for WrapConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 3000,
            inject_timeout_ms: 20_000,
        }
    }
}

/// This seat's own posture toward its guard's repository-write refusal
/// (issue #358 T8, superseding the unconditional `deny` of issues #328/
/// #334). Ordered `Allow < Advise < Deny` by declaration, the same shape
/// `workflow::deploy::DeployTier` uses for its own strictness ladder: a
/// repository layer may only TIGHTEN this (`allow` -> `advise` -> `deny`),
/// never loosen it -- see `narrow_orchestrator_writes`. `hook::run_pretool`
/// and `safety::run_check_hook_mode_with_env` both resolve this through
/// `hook::orchestrator_write_posture` before deciding what an in-scope
/// repository write actually does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrchestratorWrites {
    /// The write proceeds; no advisory, still logged so `zirv ctx status`
    /// can count it.
    Allow,
    /// The write proceeds; a rate-limited advisory note rides along in the
    /// hook's own non-blocking channel, and every occurrence is logged.
    #[default]
    Advise,
    /// Today's original behaviour (issues #328/#334): the write is refused
    /// outright, with the existing dispatch-a-worker reason text.
    Deny,
}

impl OrchestratorWrites {
    pub fn label(self) -> &'static str {
        match self {
            OrchestratorWrites::Allow => "allow",
            OrchestratorWrites::Advise => "advise",
            OrchestratorWrites::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SuperviseConfig {
    pub max_restarts: u32,
    pub poll_ms: u64,
    pub interval_secs: u64,
    pub max_cycle_secs: u64,
    pub max_failures: u32,
    pub backoff_base_secs: u64,
    pub on_failure: Option<String>,
    /// Consecutive `zirv ctx nudge`-driven restarts a single supervised run
    /// (`exec`) will honor before it starts ignoring further nudges: a
    /// separate cap from `max_restarts`, since a nudge-restart never spends
    /// that budget (it is not rot). Past the cap the nudge's mail is left
    /// unread rather than acted on, so it is still visible via `zirv ctx
    /// inbox`. Not repo-forbidden: unlike `agent_bin` or `handoff.model`,
    /// this names no binary, shell command, or model choice, only how many
    /// times a session tolerates being interrupted.
    pub max_nudges: u32,
    /// Issue #155, Phase 5(e): how many HEAVY OPERATIONS may run
    /// concurrently on this machine -- classified commands (`cargo build`/
    /// `test`/`nextest`/`clippy`/`package`/`publish`, plus
    /// `heavy_command_patterns`), each holding a permit for the duration of
    /// the child process (`permit::acquire`/`permit::HeavyPermit`), checked
    /// at `script_runner::Command::invoke`, the single seam where a zirv
    /// script runs a shell command. Replaces `max_heavy_workers`, which
    /// counted live `Verb::Exec | Verb::Dash` session records and so was
    /// blind to what those sessions were actually doing: an idle worker
    /// consumed the whole budget while a busy orchestrator running a full
    /// nextest sweep consumed none of it.
    ///
    /// `max_heavy_workers` is still accepted as a DEPRECATED ALIAS,
    /// rewritten onto this key before deserialisation: these structs are
    /// `deny_unknown_fields`, so an operator's existing `~/.zirv/ctx.toml`
    /// (or `ZIRV_CTX_SUPERVISE_MAX_HEAVY_WORKERS`) would otherwise hard-fail
    /// on upgrade. The new key wins when both are present.
    ///
    /// Defaults to 1, unchanged from issue #133: the two-parallel-worktree
    /// reproduction there needed only two concurrent cold `cargo build` +
    /// full-nextest workloads to blue-screen the host four times in twelve
    /// minutes, so the safe default is a single heavy operation at a time --
    /// an operator who has verified their own machine can take more raises
    /// this explicitly.
    ///
    /// `REPO_FORBIDDEN` under BOTH spellings, unchanged from #133: a
    /// checked-out repo raising the machine-wide concurrency budget is
    /// exactly the case the cap exists for, so only `~/.zirv/ctx.toml` or
    /// the matching `ZIRV_CTX_SUPERVISE_MAX_HEAVY_*` env var may set it.
    /// Deliberately **not** under `[agents]` -- that table is reserved for
    /// the distinct, per-agent `<repo>/.zirv/.settings.toml` gate (see
    /// `agents_in_ctx_toml_is_rejected_so_the_two_files_stay_distinct`) --
    /// this is a `[supervise]` key like every other cap in this struct.
    pub max_heavy_operations: usize,
    /// Issues #267/#338: an optional machine-wide cap on how many `--mode
    /// writing` delegated workers may hold a WRITER permit at once -- a
    /// second, independent pool from `max_heavy_operations` above. A writer
    /// permit is held for a worker's WHOLE LIFETIME (`agent::run_with`), not
    /// only while it runs one classified heavy command. Regardless of this
    /// cap, `permit::acquire_writer` never lets two writers hold the SAME
    /// checkout at once. A `--mode read-only` worker never takes a writer
    /// permit and does not count against this.
    ///
    /// Defaults to 0: no machine-wide cap, with per-tree exclusivity only.
    /// An operator who wants the coarser machine-wide policy can set a
    /// positive limit explicitly; 1 restores the original single-writer
    /// behavior.
    ///
    /// `REPO_FORBIDDEN`: whether unrelated repositories coordinate through
    /// a machine-wide writer cap is an operator policy, not something one
    /// checked-out repository may choose for the whole machine.
    pub max_writers: usize,
    /// Extra command patterns an operator classifies as heavy on their own
    /// machine, ADDED to the built-in set (`permit::BUILTIN_HEAVY_PATTERNS`),
    /// never replacing it -- `permit::is_heavy` always checks the built-ins
    /// regardless of what this holds. A repo layer may add entries (adding
    /// is narrowing), but the built-ins can never be removed by any layer.
    /// Not `REPO_FORBIDDEN`: unlike `max_heavy_operations` itself, adding a
    /// pattern can only make MORE commands wait for a permit, never fewer,
    /// so a repo checkout widening this list cannot reproduce issue #133's
    /// ungoverned-concurrency incident.
    pub heavy_command_patterns: Vec<String>,
    /// Issue #310 (3a): a session with NO PTY output, transcript growth, or
    /// mail activity for this long, while it is not inside a tool call,
    /// latches `stalled` (`stall::evaluate_progress`, `ToolState::Idle`).
    /// Mirrors the Hermes Agent reference architecture's own
    /// `_STALE_IDLE_SECONDS` (450.0).
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `max_writers`: a checked-out repo
    /// raising its own stall fuse could silently defeat the detector for a
    /// session running against it.
    pub idle_no_tool_secs: u64,
    /// Same progress clock as `idle_no_tool_secs`, applied while the session
    /// IS inside a tool call (`ToolState::InTool`) -- a stuck tool call is
    /// expected to run longer than idle "thinking" time before it counts as
    /// a stall. Mirrors Hermes's `_STALE_IN_TOOL_SECONDS` (1200.0).
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `idle_no_tool_secs`.
    pub in_tool_secs: u64,
    /// Issue #310 (3a): once the stall latch arms and the one steering
    /// nudge is sent, how long a session gets to show observed progress
    /// before it is terminated via the existing kill path. Mirrors Hermes's
    /// `_STALL_GRACE_SECONDS` (120.0).
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `idle_no_tool_secs`.
    pub stall_grace_secs: u64,
    /// Issue #379: how long a session may sit in `Attention::Compacting` (a
    /// `PreCompact` hook fired and nothing has been heard from the session
    /// since) before `attention::project_at` renders it as stalled and a
    /// dashboard pane mails its delegating session once. 600s is roughly
    /// double the slowest compaction actually observed (a codex pane at
    /// ~242K of 258K tokens took 5-6.5 minutes), so a compaction that is
    /// merely slow never trips it.
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `idle_no_tool_secs`: a checked-out
    /// repo raising its own compaction fuse could silently defeat the
    /// detector for a session running against it.
    pub compact_stall_secs: u64,
    /// Round 4 bug 2: the hard upper bound `exec`'s and `loop`'s headless
    /// in-place compaction (`exec::compact_in_place`) waits for the compact
    /// child to exit and, after that, for the transcript's own
    /// `compact_boundary` verification marker. Previously this reused
    /// `wrap.inject_timeout_ms` (20s) -- a value sized for `wrap` injecting a
    /// nudge into an already-running interactive PTY session, not for a
    /// whole model turn's worth of headless compute. A real ~150k-token
    /// compaction takes minutes, so the 20s reuse killed compactions that
    /// were actively in progress (see the production incident this field
    /// exists to fix). 600_000ms (10 minutes) mirrors `compact_stall_secs`'s
    /// own evidence: "5-6.5 minutes" is the slowest compaction actually
    /// observed elsewhere in this codebase, so 10 minutes is a safe margin
    /// above it. `compact_in_place` uses no transcript-growth stall clock at
    /// all: a single headless compaction turn writes nothing back until it
    /// completes, so growth is not a valid liveness signal for it. This bound
    /// is the only thing that can kill an in-progress compaction -- see
    /// `compact_in_place`'s own doc comment.
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `idle_no_tool_secs`: a checked-out
    /// repo shortening this could force premature restarts of a session
    /// running against it, and lengthening it could hide a truly hung
    /// compaction past its usefulness.
    pub compact_timeout_ms: u64,
    /// Issue #310 (3b): the restart-chain breaker's own trip threshold --
    /// this many unplanned, same-class respawns, each no more than
    /// `chain_max_gap_secs` apart, means "do not auto-resume, report"
    /// instead of looping forever across process boundaries
    /// (`chain::evaluate`). Mirrors Hermes's `DEFAULT_MAX_RESTARTS` (3).
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `idle_no_tool_secs`: a repo
    /// checkout raising its own restart budget could silently defeat the
    /// breaker.
    pub chain_max_restarts: u32,
    /// See `chain_max_restarts` right above. Mirrors Hermes's
    /// `DEFAULT_MAX_GAP_SECONDS` (300).
    ///
    /// `REPO_FORBIDDEN`, same reasoning as `chain_max_restarts`.
    pub chain_max_gap_secs: u64,
    /// Issue #358 T8: this seat's own posture toward its guard's refusal of
    /// a direct repository write (issues #328/#334). Defaults to `Advise`:
    /// the original `Deny` was found too restrictive on its own -- an
    /// orchestrator seat could not make even a one-line fix without a full
    /// dispatch-and-review cycle -- so the write now proceeds by default,
    /// with a rate-limited advisory and a durable count an operator can
    /// still see in `zirv ctx status`. NOT `REPO_FORBIDDEN`: unlike every
    /// other key in this struct, a repository checkout MAY narrow this
    /// (`allow` -> `advise` -> `deny`, never the reverse -- see
    /// `narrow_orchestrator_writes`), the same repo-may-only-tighten shape
    /// `pace.enabled`/`verify_on_stop.enabled` already get, because a repo
    /// asking for a stricter guard against its own orchestrator seat is
    /// exactly the direction that can never reproduce issues #328/#334.
    pub orchestrator_writes: OrchestratorWrites,
    /// Issue #311 (Hermes Agent's `/loop` self-paced mode): the ceiling
    /// `zirv ctx loop`'s own self-pacing may grow the inter-cycle wait
    /// toward when no explicit `--interval` was given and consecutive
    /// successful cycles keep producing the same outcome digest --
    /// `run_loop::next_pace`'s own `ceiling` parameter. Mirrors Hermes's
    /// `DEFAULT_SELF_PACED_CEILING_SECONDS` (900). Has no effect at all on a
    /// run launched with an explicit `--interval`: that opts out of self-
    /// pacing entirely, so this value is never consulted.
    ///
    /// Narrow-only, the same "repo may only make it stricter" shape as
    /// `compact_advisory.min_reclaim_tokens` -- but the OPPOSITE polarity:
    /// here LOWER is stricter (the loop checks in more often, waiting no
    /// longer than this many seconds between cycles even when nothing has
    /// changed), so the fold is `home.min(repo)` like `verify_on_stop.
    /// max_nudges`, not `home.max(repo)` like `compact_advisory`'s own keys.
    /// A repo checkout may shorten how long its own loop can go quiet, never
    /// lengthen it past what the operator (or another layer) already
    /// allows. Not `REPO_FORBIDDEN`: unlike `supervise.idle_no_tool_secs`/
    /// `in_tool_secs` right above (which gate a *safety* fuse a checkout
    /// must not be able to loosen), this only tunes how quickly a
    /// nothing-left-to-do loop backs off, and only in the direction that
    /// asks for MORE supervision, not less.
    pub loop_backoff_ceiling_secs: u64,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        Self {
            max_restarts: 2,
            poll_ms: 2000,
            interval_secs: 900,
            max_cycle_secs: 3600,
            max_failures: 5,
            backoff_base_secs: 60,
            on_failure: None,
            max_nudges: 3,
            max_heavy_operations: 1,
            max_writers: 0,
            heavy_command_patterns: Vec::new(),
            idle_no_tool_secs: 450,
            in_tool_secs: 1200,
            stall_grace_secs: 120,
            compact_stall_secs: 600,
            compact_timeout_ms: 600_000,
            chain_max_restarts: 3,
            chain_max_gap_secs: 300,
            orchestrator_writes: OrchestratorWrites::Advise,
            loop_backoff_ceiling_secs: 900,
        }
    }
}

/// `[hooks]` -- knobs for the PreToolUse hooks themselves, alongside
/// `[supervise] orchestrator_writes` above (the other decision
/// `hook::run_pretool` makes on the same event).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Issue #406: repository-relative path prefixes the pre-write reuse
    /// probe (`hook::run_pretool` -> `reuse::evaluate`) neither scans nor
    /// advises on -- generated code, a vendored tree, a directory whose
    /// duplication is deliberate. Empty by default, so the whole checkout is
    /// in scope.
    ///
    /// NOT `REPO_FORBIDDEN` (it is on `workflow::checks::forbidden::
    /// NARROW_ONLY_ALLOWLIST` instead): this is a SCOPE knob on an
    /// advisory-only probe that never denies a write, so a repository
    /// listing a prefix here can only make zirv say LESS, never widen what
    /// the session is allowed to do.
    pub reuse_exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    /// The operator's own choice of distiller/judgment model, when set.
    /// `None` -- the default -- means "let the adapter decide":
    /// `resolve_distiller_model` (`handoff.rs`) falls back to the resolved
    /// adapter's own `AgentAdapter::default_distiller_model`, which is a
    /// real value for claude ("haiku") but `None` for codex, since a
    /// hardcoded model name is specific to one agent's lineup and zirv has
    /// no verified cheap-model default for codex's. This used to default to
    /// the literal `"haiku"` unconditionally, which reached `codex exec
    /// --model haiku` for a codex session and failed outright.
    pub model: Option<String>,
    /// How many trailing items of each kind the handoff context keeps: user
    /// messages, assistant texts and tool errors. One knob, because
    /// `structural_context` applies one limit to all three.
    pub tail_items: usize,
    /// How long the distiller gets before the structural fallback is used
    /// instead. `wrap` calls this from its pump, so an unbounded wait would
    /// freeze the user's own terminal.
    pub timeout_secs: u64,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            model: None,
            tail_items: 5,
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaceConfig {
    pub enabled: bool,
    /// A supervised window is kept at or below this percentage.
    pub max_percent: f64,
    /// Collector readings older than this are treated as stale.
    /// `REPO_FORBIDDEN`: neither direction is a narrowing. Lengthening it
    /// keeps a reading the checkout controls binding for hours; shortening it
    /// drops a fresh vendor reading below `max_percent` out of
    /// `pace::binding`, after which the estimator's lower figure binds
    /// instead -- a repo bypassing the gate it cannot disable.
    pub collector_max_age_secs: u64,
    /// `REPO_FORBIDDEN`: the fallback source the gate paces on when no
    /// collector reading binds, measured against the budgets below -- a
    /// checkout choosing both chooses the whole reading.
    pub estimator: bool,
    /// `0` disables the estimator for that window: a plan's real allowance is
    /// undocumented, so there is no honest default. `REPO_FORBIDDEN`, same
    /// reasoning as `estimator`.
    pub five_hour_budget_tokens: u64,
    /// `REPO_FORBIDDEN`, same reasoning as `five_hour_budget_tokens`.
    pub seven_day_budget_tokens: u64,
    /// `REPO_FORBIDDEN`: cache reads are the dominant token class in a cached
    /// session, so this toggle alone moves the estimator's own percentage far
    /// enough to change a pacing verdict.
    pub count_cache_reads: bool,
    pub jitter_secs: u64,
    /// Used when a window's `resets_at` is unknown.
    pub fallback_delay_secs: u64,
    /// Head-room added to a window's own length to form the default safety cap,
    /// so a slightly wrong `resets_at` still resolves.
    pub wait_slack_secs: u64,
    /// Absolute override for the safety cap. `None` scales the cap to the window
    /// that tripped (5h or 7d, plus `wait_slack_secs`), which is what the spec's
    /// wait-until-reset semantics require: a global cap would resume early and
    /// spend tokens against a window that is still exhausted.
    pub max_wait_secs: Option<u64>,
    /// Start of the soft-throttle band. At or above this (and below
    /// `max_percent`) cycles are delayed so the remaining budget spreads
    /// linearly over the time left in the window. `>= max_percent` means no
    /// throttle band -- hard pause only.
    pub soft_percent: f64,
    /// Active API-poll fallback: only consulted when the passive collector
    /// reading is stale at a gating point.
    pub poll_enabled: bool,
    /// Per-provider floor between poll attempts, shared across processes.
    pub poll_min_interval_secs: u64,
    /// Operator declaration that a harness's vendor plan covers overage from
    /// credits: gating (throttle and pause) is skipped for that harness.
    pub use_credits: UseCreditsConfig,
    /// T8 (fail-SAFE, not open): the bounded per-cycle delay `pace::wait_for_
    /// window` applies when it is genuinely blind -- no binding collector
    /// reading, and no configured estimator to fall back on -- instead of
    /// the old behavior of skipping the gate outright and proceeding at full
    /// speed. Deliberately small next to `fallback_delay_secs`/`wait_slack_
    /// secs`: those pace a *known* trip against a *known* window, while this
    /// is a floor applied with zero visibility into actual usage, so it must
    /// not punish a single one-shot `zirv ctx agent` call (a common,
    /// legitimate case for an operator who has not wired a statusline tee)
    /// while still meaningfully slowing a tight automated loop of headless
    /// cycles that would otherwise spend against the account with nobody
    /// watching. See [[Usage and Pacing]]/[[Known Issues]].
    pub blind_delay_secs: u64,
    /// Issue #155, Phase 6(c): the soft/hard band for `pace::spawn_gate`,
    /// which gates whether a NEW delegated worker may be spawned at all
    /// (`agent::run_with`, `dash::fulfill_spawn_request`) -- never whether an
    /// already-running session gets restarted. Restarting a session because
    /// it is expensive would discard a warm cache and re-read the whole
    /// context, the single most expensive possible reaction to a cost
    /// signal, so `rot.rs`/`score.rs` never read these (or any other
    /// `pace`/`window` field) at all. Deliberately distinct from `max_
    /// percent`/`soft_percent` above, which tune an already-running
    /// supervised loop's own cadence: a spawn is new spend the operator has
    /// not yet committed to, so it earns a stricter, earlier ceiling than
    /// pacing an existing one. `REPO_FORBIDDEN`: a repo checkout must not be
    /// able to change when the operator's account stops accepting new work,
    /// in either direction.
    pub spawn_soft_pct: f64,
    /// See `spawn_soft_pct` just above. At or above this, `agent::run_with`/
    /// `dash::fulfill_spawn_request` refuse the spawn outright unless
    /// overridden (`agent::run_with`'s own `--force`, or `dash::SpawnRequest
    /// ::force` carrying that same choice into a pane spawn).
    /// `REPO_FORBIDDEN`, same reasoning as `spawn_soft_pct`.
    pub spawn_hard_pct: f64,
    /// Issue #285: the operator's own default soft token budget for a
    /// durable objective (`zirv ctx objective set`) that does not pass its
    /// own `--budget-tokens`. `None` means no default -- an objective set
    /// with no explicit budget stays unbounded, same as today. Distinct from
    /// `exec`'s own `--budget-tokens` hard stop (`EXIT_BUDGET_EXHAUSTED`):
    /// this ceiling only flips the objective's status and swaps the injected
    /// layer to the wrap-up instruction, it never kills the run.
    /// `REPO_FORBIDDEN`: a repo checkout must not be able to raise its own
    /// spend ceiling, same reasoning as `spawn_soft_pct`/`spawn_hard_pct`
    /// above.
    pub run_budget_tokens: Option<u64>,
}

impl Default for PaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_percent: 99.0,
            collector_max_age_secs: 900,
            estimator: true,
            five_hour_budget_tokens: 0,
            seven_day_budget_tokens: 0,
            count_cache_reads: false,
            jitter_secs: 30,
            fallback_delay_secs: 900,
            wait_slack_secs: 3600,
            max_wait_secs: None,
            soft_percent: 80.0,
            poll_enabled: true,
            poll_min_interval_secs: 60,
            use_credits: UseCreditsConfig::default(),
            blind_delay_secs: 60,
            spawn_soft_pct: 80.0,
            spawn_hard_pct: 95.0,
            run_budget_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UseCreditsConfig {
    pub claude: bool,
    pub codex: bool,
}

impl UseCreditsConfig {
    /// Keyed by agent in config (what the operator thinks in), resolved by
    /// provider at the gate (what pacing knows). Unknown providers gate.
    ///
    /// Called at every pacing-gate construction site (`exec`/`run_loop` build
    /// `PaceGate { use_credits: cfg.pace.use_credits.for_provider(..) }`) and
    /// by the dashboard header's per-harness usage row.
    pub fn for_provider(&self, provider: &str) -> bool {
        match provider {
            "anthropic" => self.claude,
            "openai" => self.codex,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptimizeConfig {
    /// Whether the Stop hook may queue an "optimize recommended" entry.
    pub enabled: bool,
    pub sessions_sampled: usize,
    pub max_surface_bytes: usize,
    /// Empty reuses `handoff.model`'s own resolution (`resolve_distiller_
    /// model` in `handoff.rs`, which already falls back to the resolved
    /// adapter's own default when `handoff.model` itself is unset): one
    /// cheap-model choice for the whole tool, kept as a plain `String`
    /// rather than `Option<String>` since "empty" already means "defer" here
    /// and always has.
    pub model: String,
    pub recommend_tool_failure_rate: f64,
    pub recommend_corrections: usize,
    pub recommend_cooldown_secs: u64,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sessions_sampled: 10,
            max_surface_bytes: 200_000,
            model: String::new(),
            recommend_tool_failure_rate: 0.25,
            recommend_corrections: 3,
            recommend_cooldown_secs: 86_400,
        }
    }
}

/// Issue #309: whether the Stop hook may nudge the exact stale-gate command
/// (`zirv test changed`/`zirv verify`) when the transcript shows a
/// modification this session and the last persisted verification report no
/// longer covers the current change set.
///
/// `enabled`/`max_nudges` both go through the same T9 repo-narrowing fold
/// `pace.enabled`/`context.dedupe_native` already use (`narrow_verify_on_
/// stop_enabled`/`narrow_max_nudges` below), not `REPO_FORBIDDEN`: an
/// operator who wants the nudge is never blocked by the repo, but a repo
/// checkout may only ever make the feature quieter (turn it off, or lower
/// the cap), never louder.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyOnStopConfig {
    pub enabled: bool,
    pub max_nudges: u32,
}

impl Default for VerifyOnStopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_nudges: 2,
        }
    }
}

/// Q1 (blind-review completion quality): whether the Stop hook may block a
/// HEADLESS Worker/Single session (`ZIRV_CTX_HEADLESS=1`) once when it
/// edited/created non-test source files this turn but touched no test file
/// for the change -- see `hook::missing_tests_gate_reason`'s own doc comment
/// for the detector and `hook::run_stop`'s own doc comment for every other
/// gate (interactive, `stop_hook_active`, already-blocked-this-session).
///
/// `enabled` goes through the same T9 repo-narrowing fold `verify_on_stop.
/// enabled` already uses (`narrow_missing_tests_gate_enabled` below), not
/// `REPO_FORBIDDEN`: an operator who wants the check is never blocked by the
/// repo, but a repo checkout may only ever turn it off, never force it on
/// for an operator who disabled it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MissingTestsGateConfig {
    pub enabled: bool,
}

impl Default for MissingTestsGateConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Issue #774: whether claude's `SubagentStop` hook may block a native `Task`
/// subagent's own final turn once, on a cheap deterministic result-contract
/// violation -- see `hook::run_subagent_stop`'s own doc comment for the three
/// checks and the fail-open/cap-at-one-block contract.
///
/// `enabled` goes through the identical T9 repo-narrowing fold `missing_
/// tests_gate.enabled` already uses (`narrow_subagent_stop_gate_enabled`
/// below), not `REPO_FORBIDDEN`: an operator who wants the gate is never
/// blocked by the repo, but a repo checkout may only ever turn it off, never
/// force it on for an operator who disabled it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentStopGateConfig {
    pub enabled: bool,
}

impl Default for SubagentStopGateConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The scope-creep guard: a `UserPromptSubmit`-recorded, per-session note of
/// any preservation/limitation language the request itself used (`hook::
/// record_scope_guard_request`), a non-blocking `PreToolUse` checkpoint on
/// the first `Edit`/`MultiEdit`/`NotebookEdit`/existing-file `Write` after
/// each new prompt (`hook::scope_checkpoint_note`), and a once-per-prompt
/// `Stop` backstop that blocks when the closing report claims an
/// unrequested fix the request never asked for (`hook::
/// scope_guard_stop_reason`).
///
/// `enabled` goes through the identical T9 repo-narrowing fold `missing_
/// tests_gate.enabled`/`subagent_stop_gate.enabled` already use
/// (`narrow_scope_guard_enabled` below), not `REPO_FORBIDDEN`: an operator
/// who wants the guard is never blocked by the repo, but a repo checkout may
/// only ever turn it off, never force it on for an operator who disabled it.
/// Disabled means no record is ever written, no checkpoint is ever shown,
/// and no Stop is ever blocked.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScopeGuardConfig {
    pub enabled: bool,
}

impl Default for ScopeGuardConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Issue #308 stage 1: whether the Stop hook may run a fast local checker
/// (`cargo check`/`tsc --noEmit`) after a turn that edited files, and inject
/// only the diagnostics that are NEW since this session's own baseline as one
/// bounded advisory line-block. Off by default -- unlike `verify_on_stop`
/// above, this spawns a real compiler/type-checker process on a qualifying
/// turn, a real cost an operator must opt into explicitly rather than one
/// this type defaults on.
///
/// `enabled`/`max_diagnostics`/`timeout_secs` all go through the identical T9
/// repo-narrowing fold `verify_on_stop.enabled`/`max_nudges` already use
/// (`narrow_diagnostics_enabled`/`narrow_max_diagnostics`/
/// `narrow_diagnostics_timeout_secs` below): a repo checkout may only make
/// the feature quieter or cheaper -- turn it off, lower the cap, shorten the
/// timeout -- never louder, larger, or longer-running.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    pub max_diagnostics: u32,
    pub timeout_secs: u64,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_diagnostics: 10,
            timeout_secs: 120,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let cfg = ScoreConfig::default();
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.min_turns, 10);
        assert_eq!(
            cfg.token_floor, None,
            "absolute overrides are unset by default -- see rot::token_gates"
        );
        assert_eq!(cfg.token_ceiling, None);
        assert_eq!(cfg.token_floor_ratio, 0.5);
        assert_eq!(cfg.token_ceiling_ratio, 0.8);
        assert_eq!(cfg.model_context_tokens, None);
        assert_eq!(cfg.advise_at, 40);
        assert_eq!(cfg.compact_at, 60);
        assert_eq!(cfg.restart_at, 80);
        assert_eq!(cfg.marker, "[zirv]");
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(
            cfg.weight_tool_failure + cfg.weight_repetition + cfg.weight_marker,
            100.0,
            "weights must sum to 100 so an all-signals session can reach restart"
        );
        assert_eq!(WrapConfig::default().debounce_ms, 3000);
        assert_eq!(SuperviseConfig::default().max_restarts, 2);
        assert_eq!(SuperviseConfig::default().max_nudges, 3);
        assert_eq!(
            SuperviseConfig::default().max_heavy_operations,
            1,
            "issue #133: a single heavy operation at a time is the safe default"
        );
        assert_eq!(
            SuperviseConfig::default().max_writers,
            0,
            "issue #338: 0 means no machine-wide cap, with per-tree exclusivity only"
        );
        assert_eq!(
            SuperviseConfig::default().heavy_command_patterns,
            Vec::<String>::new(),
            "the built-in set is baked into permit::is_heavy, not duplicated here"
        );
        assert_eq!(
            SuperviseConfig::default().idle_no_tool_secs,
            450,
            "issue #310: mirrors the Hermes reference's own _STALE_IDLE_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().in_tool_secs,
            1200,
            "issue #310: mirrors the Hermes reference's own _STALE_IN_TOOL_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().stall_grace_secs,
            120,
            "issue #310: mirrors the Hermes reference's own _STALL_GRACE_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().compact_stall_secs,
            600,
            "issue #379: roughly double the slowest compaction actually observed"
        );
        assert_eq!(
            SuperviseConfig::default().chain_max_restarts,
            3,
            "issue #310: mirrors the Hermes reference's own DEFAULT_MAX_RESTARTS"
        );
        assert_eq!(
            SuperviseConfig::default().loop_backoff_ceiling_secs,
            900,
            "issue #311: mirrors Hermes's own DEFAULT_SELF_PACED_CEILING_SECONDS"
        );
        assert_eq!(
            SuperviseConfig::default().chain_max_gap_secs,
            300,
            "issue #310: mirrors the Hermes reference's own DEFAULT_MAX_GAP_SECONDS"
        );
        assert_eq!(
            HandoffConfig::default().model,
            None,
            "per-adapter resolution now lives in resolve_distiller_model, not a hardcoded default"
        );
        assert_eq!(HandoffConfig::default().tail_items, 5);
        assert_eq!(HandoffConfig::default().timeout_secs, 30);
    }

    #[test]
    fn pacing_defaults_match_the_spec() {
        let pace = PaceConfig::default();
        assert!(pace.enabled, "pacing is on by default");
        assert_eq!(pace.max_percent, 99.0);
        assert_eq!(pace.collector_max_age_secs, 900);
        assert!(pace.estimator);
        assert_eq!(
            (pace.five_hour_budget_tokens, pace.seven_day_budget_tokens),
            (0, 0),
            "no invented budget: the estimator stays quiet until an operator sets one"
        );
        assert!(!pace.count_cache_reads);
        assert_eq!(pace.jitter_secs, 30);
        assert_eq!(pace.fallback_delay_secs, 900);
        assert_eq!(pace.wait_slack_secs, 3600);
        assert_eq!(
            pace.max_wait_secs, None,
            "no global cap by default: the cap is scaled to the window that tripped"
        );
    }

    #[test]
    fn pace_gains_soft_and_poll_and_use_credits_defaults() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.soft_percent, 80.0);
        assert!(cfg.poll_enabled);
        assert_eq!(cfg.poll_min_interval_secs, 60);
        assert!(!cfg.use_credits.claude);
        assert!(!cfg.use_credits.codex);
    }

    #[test]
    fn pace_gains_spawn_soft_and_hard_pct_defaults() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.spawn_soft_pct, 80.0);
        assert_eq!(cfg.spawn_hard_pct, 95.0);
    }

    #[test]
    fn pace_run_budget_tokens_defaults_to_unset() {
        assert_eq!(PaceConfig::default().run_budget_tokens, None);
    }

    #[test]
    fn use_credits_maps_providers_to_agent_flags() {
        let uc = UseCreditsConfig {
            claude: true,
            codex: false,
        };
        assert!(uc.for_provider("anthropic"));
        assert!(!uc.for_provider("openai"));
        assert!(
            !uc.for_provider("something-else"),
            "unknown provider: gate stays on"
        );
    }

    /// Serde round-trips through the documented lowercase strings, not Rust's
    /// own `Debug`/variant-name casing.
    #[test]
    fn orchestrator_writes_serializes_as_lowercase_strings() {
        for (value, text) in [
            (OrchestratorWrites::Allow, "\"allow\""),
            (OrchestratorWrites::Advise, "\"advise\""),
            (OrchestratorWrites::Deny, "\"deny\""),
        ] {
            assert_eq!(serde_json::to_string(&value).expect("serialize"), text);
            let parsed: OrchestratorWrites = serde_json::from_str(text).expect("deserialize");
            assert_eq!(parsed, value);
        }
    }

    /// Q1: default-on, unlike `diagnostics` -- an operator who never touches
    /// `missing_tests_gate` still gets the check.
    #[test]
    fn missing_tests_gate_defaults_on() {
        assert!(MissingTestsGateConfig::default().enabled);
    }

    /// Issue #774: default-on, the same as `missing_tests_gate` -- an
    /// operator who never touches `subagent_stop_gate` still gets the check.
    #[test]
    fn subagent_stop_gate_defaults_on() {
        assert!(SubagentStopGateConfig::default().enabled);
    }

    /// Default-on, the same as `missing_tests_gate`/`subagent_stop_gate` --
    /// an operator who never touches `scope_guard` still gets the guard.
    #[test]
    fn scope_guard_defaults_on() {
        assert!(ScopeGuardConfig::default().enabled);
    }

    #[test]
    fn optimize_defaults_are_conservative() {
        let optimize = OptimizeConfig::default();
        assert!(optimize.enabled, "the hook recommendation is on by default");
        assert_eq!(optimize.sessions_sampled, 10);
        assert_eq!(optimize.max_surface_bytes, 200_000);
        assert_eq!(
            optimize.model, "",
            "empty means reuse the handoff model rather than inventing a second default"
        );
        assert_eq!(optimize.recommend_tool_failure_rate, 0.25);
        assert_eq!(optimize.recommend_corrections, 3);
        assert_eq!(optimize.recommend_cooldown_secs, 86_400);
    }
}
