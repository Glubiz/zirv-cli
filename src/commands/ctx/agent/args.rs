//! CLI argument shape for `zirv ctx agent`.

use std::path::PathBuf;

use super::super::permit::WorkerMode;

#[derive(Debug, Clone, clap::Args)]
pub struct AgentArgs {
    /// Adapter name to delegate to.
    pub name: String,
    /// The task prompt, or "-" to read it from stdin.
    pub prompt: String,
    /// Extra flags for the agent's own CLI, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub flags: Vec<String>,
    /// R1-4 (2026-09-06 review): seat instructions to inject as the worker's
    /// own system prompt, in whichever form the target adapter actually
    /// supports (`AgentAdapter::system_prompt_args` -- claude's
    /// `--append-system-prompt`, codex's `developer_instructions`), rather
    /// than as a harness-specific flag the caller has to spell itself.
    ///
    /// This exists because BOTH forks of a delegation have to carry it: an
    /// inline supervised run renders it into the child's own launch flags
    /// ([`flags_with_system_prompt`]), and a dashboard pane carries it as
    /// `SpawnRequest::system_prompt` -- which survives the file-drop
    /// sanitiser, unlike the trailing `-- <flags>` a caller used to spell it
    /// in. `workflow::review::reviewer_args` is the first caller: its
    /// reviewer-seat instructions used to be dropped outright the moment a
    /// live dashboard fulfilled the review as a pane.
    #[arg(long)]
    pub system_prompt: Option<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// Suppress the `zirv ▸` announcement channel for this delegated run.
    /// Errors and warnings are never suppressed.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// What this delegated worker should present as: `worker` (the default,
    /// unstated) or `sub-orchestrator` (issue #155, Phase 5). Travels on the
    /// `SpawnRequest` (`spawnreq::role_of`) when a dashboard pane fulfils
    /// this delegation, and is re-validated there against the depth cap
    /// (`dash::mod::depth_refusal`) -- this flag is a request, not a grant.
    #[arg(long)]
    pub role: Option<String>,
    /// The work group (`zirv ctx group create`) this delegation belongs to.
    /// Its own token budget, when set, is a ceiling `--budget-tokens` may
    /// only tighten, never raise (`resolve_budget_tokens`). Unstated falls
    /// back to `WORK_GROUP_ENV` (`resolve_group_binding`, issue #170) --
    /// the group a SubOrchestrator was itself launched under, so every
    /// worker it spawns lands in that group by lineage rather than by typing
    /// `--group` on every call.
    #[arg(long)]
    pub group: Option<String>,
    /// Issue #170: what this group of delegated work is for. Meaningful only
    /// alongside `--role sub-orchestrator` and with no `--group` already
    /// resolved (explicitly or via `WORK_GROUP_ENV`): mints a fresh work
    /// group scoped to this text (`group::create`'s own defaults for
    /// everything else, `--budget-tokens` as its token ceiling) and binds
    /// this delegation to it, rather than requiring the operator to run
    /// `zirv ctx group create` as a separate step first.
    #[arg(long)]
    pub scope: Option<String>,
    /// Token ceiling for this worker (issue #155, Phase 5(d)). Checkpoints
    /// at `BUDGET_SOFT_FRACTION` of the ceiling and stops at the ceiling
    /// itself -- never a signal to change models. `None` (the default) is
    /// unbounded, exactly today's behaviour.
    #[arg(long)]
    pub budget_tokens: Option<u64>,
    /// Tool-call ceiling for this worker, independent of `--budget-tokens`.
    #[arg(long)]
    pub max_tool_calls: Option<u32>,
    /// Spend anyway at or above `pace.spawn_hard_pct`, and disable automatic
    /// cross-harness rerouting of an explicitly named agent. Travels on the
    /// `SpawnRequest` so headless and dashboard-pane delegations honor the
    /// same operator override.
    #[arg(long, default_value_t = false)]
    pub force: bool,
    /// Issue #228: a harness-agnostic working directory for this worker,
    /// independent of the delegating session's own repo. Canonicalised and
    /// validated up front (`validate_workdir`): must already exist, be a
    /// directory, and sit inside a git repository -- there is no escape
    /// hatch for one that is not. Honoured by both forks of this
    /// delegation: a headless spawn's child process cwd and per-harness
    /// sandbox derive from it exactly as they otherwise derive from the
    /// current directory (`run_with`'s own `launch_repo`), and a pane spawn
    /// carries it on the `SpawnRequest` (`spawnreq::SpawnRequest::workdir`)
    /// for the dashboard to launch into and widen the pane's filesystem
    /// policy to, once re-validated there.
    #[arg(long)]
    pub workdir: Option<PathBuf>,
    /// Issue #267: whether this worker may write to its checkout. Default
    /// `writing` -- a wrong `read-only` silently drops real edits, which is
    /// worse than a wrong `writing` holding a writer-permit slot it did not
    /// need. `writing` holds a writer permit ([`permit::acquire_writer`])
    /// for the worker's whole lifetime, exclusive per checkout (see
    /// `--worktree` below); `read-only` never takes one. Travels on the
    /// `Delegation` row this run logs (`log::Delegation::mode`); the
    /// workflow engine classifies Review as `read-only` and Test/Verify as
    /// `writing` (`workflow::engine::auto_spawn_decision`). Codex read-only
    /// denies build artifact and cache writes, so build/test commands fail.
    #[arg(long, value_enum, default_value_t = WorkerMode::Writing)]
    pub mode: WorkerMode,
    /// Issue #267: allocates a fresh `git worktree add` sibling of `repo` at
    /// `<repo>/.zirv/worktrees/<short>` and uses it as this worker's own
    /// `--workdir` -- the escape hatch from `--mode writing`'s per-tree
    /// exclusivity, for a second writing worker that genuinely needs to run
    /// concurrently with one already holding `repo` (or another tree).
    /// Mutually exclusive with an explicit `--workdir`: allocating a fresh
    /// tree and being told to use a specific existing one are two different
    /// requests, and honouring one over the other silently would surprise
    /// whichever the operator actually meant. Meaningless -- and never
    /// consulted -- for `--mode read-only`, which takes no writer permit and
    /// so has no tree to isolate.
    #[arg(long, default_value_t = false)]
    pub worktree: bool,
    /// Materialize a named `[[workspace]]` from `.zirv/ctx.toml` before the
    /// harness worker launches. The selected workspace may declare extra git
    /// repositories, required MCP server names, skills, and ordered setup
    /// commands. All are strict pre-launch gates. With `--worktree`, they are
    /// materialized inside the fresh linked tree; otherwise the current
    /// checkout (or explicit `--workdir`) is the workspace root.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Prepare this checkout for the operator's stated goal before the main
    /// worker launches. Harness runtime only.
    #[arg(long)]
    pub goal: Option<String>,
    /// Internal synchronous-dispatch request for callers that must consume
    /// the completed result before they can continue. No CLI spelling.
    #[arg(skip)]
    pub(crate) inline: bool,
    /// Internal result of `--manifest agent:` resolution. This has no CLI
    /// spelling: it is intentionally populated only by the untrusted
    /// delegation-manifest merge, then used for skill defaults.
    #[arg(skip)]
    pub manifest_agent: Option<String>,
    /// Issue #718: opt-in warm-worktree reuse for `--worktree` -- before
    /// minting a fresh `git worktree add`, `allocate_worktree` looks for an
    /// `Idle` pooled tree whose recorded base commit (hashed with
    /// `worktree::setup_digest`) matches this call's, reusing it via `git
    /// reset --hard` instead. Off by default: silently handing a worker a
    /// previously-used tree is a bigger behavior change than a bounded
    /// finding should force on every `--worktree` caller. Meaningless, and
    /// never consulted, without `--worktree`.
    #[arg(long, default_value_t = false)]
    pub worktree_reuse: bool,
    /// Attach the repo's accepted workflow artifact for this stage to the
    /// worker's task prompt: resolves `--workflow` (or the repo's own
    /// active workflow when unstated), reads its accepted intent/spec/plan
    /// via `workflow::engine::read_accepted_artifact`, and appends a capped,
    /// explicitly labeled excerpt after the operator's own prompt text (see
    /// [`resolve_attached_artifact`]). `None` (the default) is today's
    /// behaviour, byte for byte unchanged -- nothing is read, nothing is
    /// appended. Fails the delegation outright, before any worker launches,
    /// when no workflow is active/named or the resolved stage has nothing
    /// accepted yet: a worker silently sent off without the context the
    /// operator explicitly asked to attach would burn a whole run on stale
    /// assumptions rather than fail loudly up front.
    #[arg(long, value_enum)]
    pub attach_artifact: Option<ArtifactStageArg>,
    /// Which workflow `--attach-artifact` reads its artifact from. Unstated
    /// resolves the repo's own active workflow (`workflow::engine::
    /// load_active`). Meaningless, and never consulted, without `--attach-
    /// artifact`.
    #[arg(long)]
    pub workflow: Option<String>,
    /// Issue #264: what KIND of work this delegation is, for the cost
    /// ledger's own `task_class` (`log::Delegation::task_class`) -- later
    /// analysis and routing groups outcomes by kind of work, not only by
    /// harness or model. `None` (the default, unstated) means "unclassified",
    /// exactly what every delegation before this flag existed already was.
    /// The workflow engine's own auto-spawned review/test/verify workers set
    /// this from the step that spawned them (`workflow::engine::auto_spawn_
    /// decision`) rather than requiring an operator to type it.
    #[arg(long, value_enum)]
    pub task_class: Option<super::super::log::TaskClass>,
    /// Issue #318: a structural contract this worker's final report is held
    /// to -- a path to a JSON schema file, or the schema as inline JSON
    /// text (see `result_schema::Schema::from_json` for the shape).
    /// Appended to the worker's own prompt as an OUTPUT CONTRACT block
    /// (`resolve_result_schema`/`attach_result_contract_to_prompt`) and
    /// exported into its env (`RESULT_SCHEMA_ENV`) so a pane's own `zirv
    /// ctx send` self-report is held to it too. Mutually exclusive with
    /// `--result-kind`: one names a schema outright, the other names a
    /// built-in one, and honouring both would leave it ambiguous which
    /// actually governs the contract.
    #[arg(long, conflicts_with = "result_kind")]
    pub result_schema: Option<String>,
    /// Issue #318: the same contract as `--result-schema`, named from one
    /// of the built-in shapes (`result_schema::BUILT_IN_KINDS`) instead of
    /// typed out by hand.
    #[arg(long, conflicts_with = "result_schema")]
    pub result_kind: Option<String>,
    /// Issue #262: an additional write root this worker's delegation
    /// envelope may narrow to, repeatable. Each value must already be
    /// contained by the parent's own envelope
    /// (`envelope::WorkerEnvelope::narrow`) or the delegation is refused
    /// before any spawn. Named `--path-scope`, not `--scope`: `--scope`
    /// above already names the work-group purpose text (issue #170).
    /// Unstated defers to the parent's own paths (`--mode writing`) or no
    /// write roots at all (`--mode read-only`, which holds no writer permit
    /// and so needs none).
    #[arg(long = "path-scope")]
    pub path_scope: Vec<PathBuf>,
    /// Issue #262: denies network tools in this worker's delegation
    /// envelope, regardless of what the parent envelope allowed. Only ever
    /// narrows: unstated leaves the parent's own `network` value untouched.
    #[arg(long, default_value_t = false)]
    pub no_network: bool,
    /// Issue #262: how many further hops of `zirv agent` this worker's own
    /// envelope should carry. Unstated defers to `parent.delegation_depth -
    /// 1`, the automatic decrement every delegation gets; an explicit value
    /// above that ceiling is refused before any spawn
    /// (`envelope::CannotGrow`), never silently clamped.
    #[arg(long)]
    pub depth: Option<u8>,
    /// Issue #317: the durable task-card id (`zirv ctx task create`/`zirv ctx
    /// swarm`) this delegation fulfils. Resolved and claimed before any
    /// spawn -- refused, with nothing launched, when the card cannot be
    /// claimed (unmet parents, or already running elsewhere under a live
    /// claimant). Every parent card's own `outcome`, plus this card's
    /// `brief`, are appended verbatim (labelled) after the operator's own
    /// prompt text. On completion the card is marked `Done` from the actual
    /// report-back (never a synthetic success just because the process
    /// exited 0); a crash or an unvalidated report-back applies `task::
    /// respawn_decision` instead of ever marking it `Done` silently.
    #[arg(long)]
    pub task: Option<String>,
    /// Issue #725: a YAML file describing this delegation declaratively,
    /// instead of the long flag list above -- `brief`/`task`/`group`/
    /// `workdir`/`mode`/`budget_tokens`/`max_tool_calls`/`path_scope`/
    /// `no_network`/`result` (`agent_manifest::apply`, the one place this
    /// is resolved, called at the very top of `run_with` before anything
    /// else reads `args`). UNTRUSTED input, exactly like any other
    /// repo-owned surface: resolved into these same `AgentArgs` fields and
    /// nothing else, then validated through the unchanged existing gates
    /// below -- it can never grant more than the same flags typed on the
    /// CLI could. A field both the manifest and an explicit CLI flag name
    /// is a hard error when they disagree, except the narrowing-capable
    /// fields (`no_network`, `budget_tokens`, `max_tool_calls`,
    /// `path_scope`, read-only `mode`), where the stricter value always
    /// wins regardless of source. Relative paths inside the manifest
    /// (`workdir`, `result.schema`, `path_scope`) resolve against the
    /// manifest file's own directory. An optional `agent: <AgentManifest
    /// id>` YAML field supplies skill defaults and capability floors; it has
    /// no direct CLI spelling.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Issue #452: print a machine-readable delegation receipt instead of
    /// the human lines -- see [`DelegationReceipt`]. Exit codes are
    /// unchanged; this only changes what reaches stdout.
    #[arg(long)]
    pub json: bool,
    /// Issue #479 (roadmap N10): which runtime drives this WORKER's own
    /// conversation -- `harness` (the default: zirv supervises an external
    /// coding-agent process, byte for byte today's behaviour) or `native`
    /// (zirv conducts the model/tool conversation itself over a direct
    /// provider route, with no coding harness installed at all). Same flag
    /// shape and same two values as `zirv ctx exec --runtime`, and equally
    /// explicit: native is never selected by detection.
    ///
    /// `--runtime native` re-reads the positional `<name>` as the native
    /// ROUTE to spend rather than a harness to launch -- a native worker has
    /// no harness to name -- with the reserved value `native` meaning "the
    /// `[roles]` entry for `--role`". [`AgentArgs::route`] overrides it.
    /// Everything else about the delegation is unchanged: the same task
    /// claim, worktree allocation, writer permit, envelope narrowing, token
    /// reservation, result contract, receipt and report-back mail.
    ///
    /// The default, `configured` (issue #491), means "whatever `[runtime]` in
    /// `~/.zirv/ctx.toml` says for this `--role`, harness when it says
    /// nothing"; `zirv ctx agent::run` resolves it to one of the two literal
    /// values before anything else in this module sees it.
    #[arg(long, default_value = super::super::runtime::CONFIGURED)]
    pub runtime: String,
    /// Native runtime only: which `[route]` from the operator's own native
    /// provider configuration this worker spends, overriding the positional
    /// `<name>`. Mirrors `zirv ctx exec --route`.
    #[arg(long)]
    pub route: Option<String>,
    /// Internal conversation identity selected by the delegation service.
    #[arg(skip)]
    pub session_id: Option<String>,
    /// Internal cancellation shared with the delegation service.
    #[arg(skip)]
    pub cancellation: Option<std::sync::Arc<super::super::provider::adapter::CancellationFlag>>,
}

/// Programmatic callers need explicit runtime defaults so an unset string cannot select the runtime.
impl Default for AgentArgs {
    fn default() -> Self {
        Self {
            name: String::new(),
            prompt: String::new(),
            flags: Vec::new(),
            system_prompt: None,
            max_restarts: None,
            timeout_secs: None,
            quiet: false,
            role: None,
            group: None,
            scope: None,
            budget_tokens: None,
            max_tool_calls: None,
            force: false,
            workdir: None,
            mode: WorkerMode::Writing,
            worktree: false,
            workspace: None,
            goal: None,
            inline: false,
            manifest_agent: None,
            worktree_reuse: false,
            attach_artifact: None,
            workflow: None,
            task_class: None,
            result_schema: None,
            result_kind: None,
            path_scope: Vec::new(),
            no_network: false,
            depth: None,
            task: None,
            manifest: None,
            json: false,
            runtime: super::super::runtime::RuntimeKind::Harness.to_string(),
            route: None,
            session_id: None,
            cancellation: None,
        }
    }
}

/// Keep clap derives local to the delegation flag instead of coupling the workflow engine to CLI parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ArtifactStageArg {
    Intent,
    Spec,
    Plan,
}

impl std::fmt::Display for ArtifactStageArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Intent => "intent",
            Self::Spec => "spec",
            Self::Plan => "plan",
        })
    }
}

impl From<ArtifactStageArg> for crate::commands::workflow::engine::ArtifactStage {
    fn from(value: ArtifactStageArg) -> Self {
        use crate::commands::workflow::engine::ArtifactStage;
        match value {
            ArtifactStageArg::Intent => ArtifactStage::Intent,
            ArtifactStageArg::Spec => ArtifactStage::Spec,
            ArtifactStageArg::Plan => ArtifactStage::Plan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-06: `--headless` is gone as a spawn topology, so the flag must
    /// not parse at all -- a script still passing it fails loudly rather than
    /// being silently ignored and getting the opposite of what it asked for.
    #[test]
    fn the_headless_flag_no_longer_parses() {
        use clap::Parser;

        #[derive(Debug, clap::Parser)]
        struct OnlyAgent {
            #[command(flatten)]
            args: AgentArgs,
        }

        assert!(
            OnlyAgent::try_parse_from(["zirv", "claude", "go"]).is_ok(),
            "the delegation itself still parses"
        );
        let err = OnlyAgent::try_parse_from(["zirv", "claude", "go", "--headless"])
            .expect_err("--headless must be rejected outright");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "got {err}"
        );
    }
}
