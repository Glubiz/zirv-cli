use std::path::{Path, PathBuf};
use std::process::Command;

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod droid;
pub mod gemini;
pub mod goose;
pub mod grok;
pub mod kimi;
pub mod muse;
pub mod opencode;
pub mod pi;
pub mod qwen;

use super::CtxResult;
use super::config::{CtxConfig, OrchestratorWrites};
use super::event::{
    Capabilities, NormalizedEvent, ProviderErrorClass, SessionId, SessionRef, StructuralContext,
    TranscriptUsage,
};

mod dirs;
mod env;
mod error;
mod launch_policy;
mod model;
mod probe;
mod roster;

pub(super) use dirs::{resolve_home_dir, resolve_state_dir};
pub use env::{
    AGENT_ENV, HEADLESS_ENV, INTERNAL_ENV, LAUNCH_MODE_ENV, LAUNCH_MODE_INTERACTIVE_VALUE,
    LaunchMode, PROXY_DECIDED_ENV, SEAT_MODEL_ENV, SEAT_ROLE_ENV, SESSION_ENV, SOCKET_ENV,
    TurnSignalSetup, headless_marker_env, launch_mode_pin_env, seat_model_env, seat_role_env,
};
pub(crate) use env::{ModelFlagForm, classify_model_flag, last_model_flag, model_only_flags};

/// Openers of user-role text a harness or zirv injects, as found in real Codex rollouts and
/// Claude transcripts. Anything else, including a request that starts with `<Button>`, is the
/// operator's.
const INJECTED_USER_TEXT: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<recommended_plugins>",
    "<turn_aborted>",
    "<guardian_context_omission>",
    "<system-reminder>",
    "<task-notification>",
    "<command-name>",
    "<command-message>",
    "<local-command-",
    "<user-prompt-submit-hook>",
    "Stop hook feedback",
    "Supervisor ruling",
];

/// Whether `text` is context injected into a transcript's user role, not an operator request.
pub(crate) fn is_injected_user_text(text: &str) -> bool {
    let text = text.trim_start();
    INJECTED_USER_TEXT
        .iter()
        .any(|opener| text.starts_with(opener))
}
pub(crate) use error::{
    ProviderErrorHints, classify_provider_error, provider_error_id, redacted_tool_summary,
};
pub use launch_policy::{
    SHIPPED_POSTURE_ALLOW, SHIPPED_POSTURE_ASK, SHIPPED_POSTURE_DENY, extend_read_only_args,
    flags_pin_policy, floorless_adapter_names, policy_launch_args, policy_launch_args_for_surface,
    read_only_args_for_agent_name, require_read_only_floor, with_workload_writable_roots,
};
pub(crate) use launch_policy::{doubled_slash_rule_base, scratchpad_roots, scratchpad_rules};
pub use model::{
    provider_for_agent_and_model, provider_for_agent_name, provider_for_usage_readout,
    worker_effort_args, worker_model_args,
};
pub(crate) use model::{resolve_review_model, resolve_tiered_model};
pub use probe::ProbeCache;
pub(crate) use probe::{
    CMD_REPARSE_METACHARS, Liveness, built_args, format_launch_error, liveness_probe,
    pin_newest_transcript, refuse_if_program_absent_with_presence,
};
#[cfg(test)]
pub(crate) use probe::{ProbeCacheFile, everything_installed, nothing_decidable, only_installed};
pub use probe::{
    ResolvedProgram, flatten_command, guard_cmd_shim_reparse, launch_reparses_through_shim,
    launches_through_cmd_shim, program_invocation, program_is_present, resolve_program,
};
#[cfg(test)]
pub(crate) use roster::READINESS_NOTE_CALLS;
#[cfg(test)]
pub use roster::harness_prompt_lines;
pub use roster::{HarnessRosterReport, harness_prompt_lines_cached, readiness_note};
pub(crate) use roster::{adapter_liveness, adapter_liveness_with};

/// `Debug` is a supertrait so `Box<dyn AgentAdapter>` can appear in
/// `Result::expect_err` (the registry tests assert on the unknown-adapter
/// error path); every adapter already derives it.
pub trait AgentAdapter: std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// Names of MCP servers the wrapped harness will actually resolve for
    /// this launch. Workspace binding uses this as a strict pre-spawn gate:
    /// a declaration is a requirement, never a hint zirv may silently drop.
    ///
    /// The default delegates to the adapter-specific config readers in
    /// `ctx::workspace` (Claude and Codex today). A future adapter must add a
    /// verified reader there or override this method; an unknown format
    /// fails closed rather than guessing. Secret values are never returned.
    fn configured_mcp_servers(
        &self,
        repo: &Path,
        flags: &[String],
        env: super::config::EnvLookup<'_>,
    ) -> CtxResult<std::collections::BTreeSet<String>> {
        super::workspace::configured_mcp_servers(self.name(), repo, flags, env)
    }

    /// The program this adapter actually spawns -- `agent_bin`'s override, or
    /// this adapter's own default binary name, whichever `ClaudeAdapter::new`/
    /// `CodexAdapter::new` resolved to `program` at construction. Distinct
    /// from `name()`: `name` is the fixed registry key (`"claude"`,
    /// `"codex"`), while this can be any override an operator's `agent_bin`
    /// or `--agent-bin` named. Exists so a caller with only a `&dyn
    /// AgentAdapter` -- `harness_prompt_lines`'s presence check in particular
    /// -- can ask what binary `ready()` actually resolved, without the
    /// module-private `program` field each adapter otherwise keeps to itself.
    fn program(&self) -> &str;

    /// Env var NAMES the harness itself authenticates with, kept when a delegated worker's
    /// secret-shaped env is scrubbed. `None` = not declared: that harness's env is left alone.
    fn credential_env(&self, _env: super::config::EnvLookup<'_>) -> Option<Vec<String>> {
        None
    }

    /// The ACCOUNT/vendor whose rate limits this agent spends, as a stable
    /// lowercase slug (`[a-z0-9-]`): `"anthropic"` for claude, `"openai"`
    /// for codex.
    ///
    /// Deliberately *not* the binary or the adapter's own `name`. Usage
    /// windows are a property of the account being billed, and two harnesses
    /// can sit on one account -- a second Anthropic-backed harness would
    /// report `"anthropic"` here and share claude's windows, which is the
    /// truth about the limit even though it is a different program. It is
    /// what `StateDir::usage_for` names a usage file after, so a change to an
    /// existing adapter's slug orphans that adapter's stored readings.
    fn provider(&self) -> &'static str;

    /// The ACCOUNT a LAUNCH actually spends, given the `model` it pins --
    /// the per-launch sibling of [`provider`](Self::provider) above.
    ///
    /// Default: this adapter's own static `provider()`, ignoring `model`
    /// entirely -- correct for claude and codex, whose account never
    /// depends on which model argv names. A multi-provider adapter (one CLI
    /// whose `provider/model` argument can pin different vendors -- an
    /// OpenCode/Pi/Goose/Droid-shaped front end) overrides this to read the
    /// vendor prefix out of `model` instead, so `StateDir::usage_for`/
    /// `poll_marker_for`, pacing, and the token-reservation ledger all file
    /// this launch under the account it is actually billed to rather than
    /// under one static slug shared by every model the adapter can launch.
    /// `model: None` (no pinned/resolved model in hand at the call site)
    /// always falls back to `provider()`.
    fn provider_for_model(&self, model: Option<&str>) -> &'static str {
        let _ = model;
        self.provider()
    }

    /// Issue #395 (operator-only endpoint overrides): attaches `endpoint` --
    /// resolved from `[endpoint.claude]`/`[endpoint.codex]` in the
    /// operator's own `~/.zirv/ctx.toml`, never a repo layer (see `config.rs`'s
    /// whole-table `REPO_FORBIDDEN` entry for `endpoint`) -- to this adapter
    /// INSTANCE, so `provider()`, `ready()`'s credential-presence check,
    /// `base()`'s launch-time wiring (claude's `ANTHROPIC_BASE_URL`/
    /// `ANTHROPIC_AUTH_TOKEN` env, codex's `-c model_provider=...` argv) and
    /// `model_args`'s vendor-ladder pinning all read the SAME resolved
    /// target rather than re-deriving it independently. `select`/
    /// `resolve_default` call this exactly once, right after constructing
    /// the adapter and before `ready()` -- so a missing credential fails the
    /// same pre-spawn check a genuinely unlaunchable binary already does,
    /// not a later step. Default no-op: every adapter this override cannot
    /// name (anything but claude/codex, since `[endpoint.<agent>]` schema
    /// only has those two leaves) never overrides it and is unaffected.
    fn apply_endpoint(&mut self, endpoint: Option<&super::config::EndpointTarget>) {
        let _ = endpoint;
    }

    /// Issue #504 (operator-only interactive permission mode): attaches
    /// `cfg.chat` to this adapter INSTANCE, mirroring `apply_endpoint`
    /// immediately above -- `select`/`resolve_default` call this exactly
    /// once, right after constructing the adapter, so `ClaudeAdapter::
    /// default_sandbox_args`'s interactive `--permission-mode` argv reads
    /// the same resolved `chat.claude_permission_mode` every other read of
    /// this adapter instance does. Default no-op: only `ClaudeAdapter`
    /// overrides it today; codex has no equivalent flag.
    fn apply_chat_config(&mut self, chat: &super::config::ChatConfig) {
        let _ = chat;
    }

    /// Issue #788 (operator-only headless cost levers): attaches
    /// `cfg.headless` to this adapter INSTANCE, mirroring `apply_chat_config`
    /// immediately above -- `select`/`resolve_default` call this exactly
    /// once, right after constructing the adapter, so `ClaudeAdapter::
    /// default_sandbox_args`/`launch_settings_path`'s HEADLESS-only
    /// `--disallowedTools`/lean-settings additions read the same resolved
    /// `[headless]` table every other read of this adapter instance does.
    /// Default no-op: only `ClaudeAdapter` overrides it today; codex has no
    /// equivalent lever.
    fn apply_headless_config(&mut self, headless: &super::config::HeadlessConfig) {
        let _ = headless;
    }

    /// Issue #840: attaches `cfg.approvals`; only the Claude adapter has a `PermissionRequest` hook to time.
    fn apply_approvals_config(&mut self, approvals: &super::config::ApprovalsConfig) {
        let _ = approvals;
    }

    /// Issue #395: the catalogue vendor slug of this adapter INSTANCE's own
    /// attached endpoint override, or `None` when it has none -- what
    /// `harness_prompt_lines`'s roster line and `zirv ctx status` render as
    /// `(endpoint: <vendor>)`, without either caller needing to know the
    /// concrete adapter type behind a `&dyn AgentAdapter`. Default `None`;
    /// only claude/codex override it, mirroring `provider()`.
    fn endpoint_vendor(&self) -> Option<&str> {
        None
    }

    /// `Err` when the adapter exists but is not safe to use yet, so callers
    /// fail loudly instead of scoring garbage.
    fn ready(&self) -> CtxResult<()>;

    /// Whether a `ready()` failure (if any) is a permanent fact of the
    /// platform this binary is running on, rather than a transient "not
    /// installed/configured yet" state. `readiness_note()`'s "Not ready yet:
    /// ... (see issue #11)" clause reads as "go install this," which is
    /// false for a harness that can never run on this OS at all --
    /// distinguishing the two lets that function route such an adapter into
    /// its own "Unsupported on this platform" clause instead. Default
    /// `false`: every adapter but muse (issue #394, macOS/Linux only -- see
    /// `MuseAdapter::ready`) fails `ready()` only for reasons a user CAN fix
    /// (missing binary, missing credential, an unlaunchable `.cmd`/`.py`
    /// shim), so only muse overrides this.
    fn platform_unsupported(&self) -> bool {
        false
    }

    fn detect(&self, command: &[String]) -> bool;

    fn headless_cmd(&self, prompt: &str, session: &SessionId, extra: &[String]) -> Command;
    /// Builds a headless prompt against an existing conversation. `prompt`
    /// is `None` when the caller will deliver it on stdin. The default is an
    /// honest refusal: adapters must not guess a resume flag or claim stdin
    /// support they have not verified.
    fn headless_resume_cmd(
        &self,
        prompt: Option<&str>,
        session_id: &str,
        extra: &[String],
    ) -> Option<Command> {
        let _ = (prompt, session_id, extra);
        None
    }
    /// The identifier a headless resume must actually target for `session`:
    /// zirv's own minted id by default, since every adapter whose resume
    /// flag takes that id directly (claude's `--resume`, qwen's `--resume`)
    /// needs no translation at all. An adapter whose CLI mints its own,
    /// unrelated session id (codex: `headless_cmd`'s own doc comment already
    /// establishes it ignores zirv's id entirely) overrides this to look one
    /// up and returns `None` when it cannot be recovered. Callers -- see
    /// `exec::headless_resume_launch`, the sole consumer -- must fail closed
    /// on `None` rather than fall back to guessing (never an adapter's own
    /// "most recent session" shorthand, which races any other session of
    /// that same adapter live in the same repo).
    fn resume_target(&self, session: &SessionRef) -> Option<String> {
        Some(session.id.as_str().to_string())
    }
    /// Whether this adapter can compact and then resume the same headless
    /// conversation. False unless an adapter has verified both halves of the
    /// operation; headless supervisors use this before spending a compact
    /// attempt or stopping the child for one.
    fn supports_headless_compact(&self) -> bool {
        false
    }
    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command;
    /// Builds the judgment/distiller model child's command. `model` is empty
    /// when neither the operator's own config (`handoff.model`/`optimize.
    /// model`) nor this adapter's own [`default_distiller_model`](Self::
    /// default_distiller_model) named one, which an adapter with no sane
    /// default of its own (codex) must read as "omit the model flag
    /// entirely" rather than pass an empty value to its own CLI -- see
    /// `resolve_distiller_model` in `handoff.rs`, which is what every caller
    /// uses to turn `Option<&str>` config into this parameter.
    fn distiller_cmd(&self, model: &str) -> Command;

    /// The flags that keep this agent from writing files or running shell
    /// commands: claude's `--disallowedTools=...`, codex's `--sandbox
    /// read-only`. This is the pin `distiller_cmd` applies, exposed on the
    /// trait so any other child that embeds untrusted repository text in its
    /// prompt (the workflow reviewer, which is handed a repo diff) applies the
    /// *same* restriction instead of a hardcoded copy that can drift from it.
    ///
    /// No default: a new adapter has to answer this deliberately rather than
    /// inherit "no restriction" by omission.
    fn read_only_args(&self) -> Vec<String>;

    /// Use only restrictions accepted by the interactive CLI surface; exec-only flags can terminate a pane at launch.
    fn interactive_read_only_args(&self) -> Vec<String> {
        self.read_only_args()
    }

    /// Whether a read-only floor can be enforced for this surface, answered without the side effects
    /// (policy or config files) some adapters' `read_only_args` materialize; refusal and routing use this.
    fn read_only_floor_available(&self, interactive: bool) -> bool {
        let floor = if interactive {
            self.interactive_read_only_args()
        } else {
            self.read_only_args()
        };
        !floor.is_empty()
    }

    /// Build a provider-neutral workflow-seat launch. Agent manifests describe
    /// required capabilities and methodology but never grant authority: this
    /// default re-loads the effective canonical policy, applies the normal
    /// headless sandbox/policy projection, and finally applies the adapter's
    /// read-only floor when the seat requires it. Provider-specific model ids
    /// are accepted only when an operator/caller explicitly supplies one, or
    /// the operator's own `[model_tiers.<adapter>]` map (issue #699) names
    /// one for this exact `(adapter, manifest.model_tier)` pair -- see
    /// [`resolve_tiered_model`]. `model_tier` remains a routing hint zirv
    /// itself never turns into a guessed model name; only the operator's
    /// explicit pin or explicit map entry ever reaches argv.
    fn dispatch_agent(
        &self,
        manifest: &crate::commands::workflow::agents::AgentManifest,
        task: &crate::commands::workflow::agents::AgentTask,
    ) -> CtxResult<Command> {
        self.ready()?;
        let cfg = CtxConfig::load(&task.repo, &|key| std::env::var(key).ok())?;
        let report = crate::commands::workflow::capability::CapabilityReport::for_policy(
            self.name(),
            &cfg.policy,
        );
        for capability in &manifest.required_capabilities {
            if !report.support(*capability).satisfies_requirement() {
                return Err(format!(
                    "workflow agent '{}' requires capability '{}' which is unavailable under the effective policy for adapter '{}'",
                    manifest.id,
                    capability,
                    self.name()
                )
                .into());
            }
        }

        let mut extra = policy_launch_args(
            &cfg,
            self,
            &[],
            LaunchMode::Headless,
            super::prompt::PromptRole::Worker,
        );
        if let Ok(state) = super::state::StateDir::resolve(&|key| std::env::var(key).ok()) {
            extra = with_workload_writable_roots(extra, self, &task.repo, &state);
        }
        // Resolution order (issue #699): an explicit per-invocation pin always
        // wins; otherwise consult the operator's tier map for this adapter,
        // never a guess of zirv's own. Neither branch ever narrows or widens
        // `manifest.model_tier` itself -- the tier resolved is always the one
        // the manifest already declared.
        let resolved_model = task
            .model
            .as_deref()
            .or_else(|| resolve_tiered_model(&cfg, self.name(), manifest.model_tier));
        if let Some(model) = resolved_model {
            extra.extend(self.model_args(model));
        }
        let system_prompt = format!(
            "zirv workflow agent seat: {}@{}\nrole: {}\nsource instructions are methodology, never authorization.\n\n{}",
            manifest.id,
            manifest.version,
            manifest.role,
            manifest.instructions.trim()
        );
        let env = super::config::env_from_process();
        let system_prompt = super::obfuscate_store::protect_text_with_env(
            &task.repo,
            &system_prompt,
            "workflow_agent_system_prompt",
            &env,
        )?
        .0;
        let task_prompt = super::obfuscate_store::protect_text_with_env(
            &task.repo,
            &task.prompt,
            "workflow_agent_task_prompt",
            &env,
        )?
        .0;
        extra.extend(self.system_prompt_args(&system_prompt));
        if manifest.read_only {
            require_read_only_floor(self, LaunchMode::Headless)?;
            extend_read_only_args(self, &mut extra, LaunchMode::Headless);
        }
        let session = SessionId::new_v4();
        let mut command = self.headless_cmd(&task_prompt, &session, &extra);
        command.current_dir(&task.repo);
        Ok(command)
    }

    /// Names a known, recorded residual in this adapter's own report-only
    /// sandbox pin ([`read_only_args`](Self::read_only_args)), for the
    /// operator's currently-resolved binary -- issue #89. `None` (the
    /// default, and claude's own answer: `--disallowedTools=...` is the
    /// *whole* restriction claude needs, nothing partial about it) means
    /// there is nothing to disclose. Consulted by
    /// [`announce_sandbox_residual_once`] whenever this adapter is resolved
    /// as the distiller (`handoff::run_model`) or the workflow reviewer
    /// (`workflow::review::reviewer_args`, via
    /// [`read_only_args_for_agent_name`]), so an operator whose judgment/
    /// review child runs on codex learns about the residual instead of
    /// discovering it only in a doc file a terminal session never opens.
    fn sandbox_residual_note(&self) -> Option<String> {
        None
    }

    /// The model name to use for the judgment/distiller child when the
    /// operator has not named one explicitly (`handoff.model`/`optimize.
    /// model` both empty/unset). `None` -- the default, and codex's own
    /// answer -- means this adapter has no verified cheap-model default of
    /// its own to guess, so `resolve_distiller_model` passes an empty model
    /// through, and this adapter's own `distiller_cmd` must read that as
    /// "omit the model flag" so the agent's own configuration (e.g. codex's
    /// `~/.codex/config.toml`) picks a model instead of zirv guessing a name
    /// that may not exist on the operator's account. Claude's own default is
    /// a real, verified value ("haiku") rather than the trait default,
    /// because a hardcoded model name is specific to one agent's lineup and
    /// must never leak into another adapter's guess.
    fn default_distiller_model(&self) -> Option<&'static str> {
        None
    }

    /// This adapter's own verified model ladder, one tier below `seat` --
    /// `seat` is the orchestrator seat's own model (`cfg.chat.model`), or
    /// `None`/unrecognised when unset, which this must read as "assume the
    /// top tier" rather than guess low. Used only when the operator has not
    /// set `review.<agent>` explicitly (see `resolve_review_model` below,
    /// the one place this and the operator override are combined into the
    /// harness-roster line an Orchestrator session sees).
    ///
    /// `""` -- the default, and not meant to be a real model id -- means
    /// this adapter has no verified ladder of its own, the same "nothing
    /// verified to guess" answer `default_distiller_model`'s `None` gives.
    /// Both registered adapters (claude, codex) override this with real,
    /// verified tier names; `resolve_review_model` is the only caller, and
    /// treats a `""` result the same way it treats any other resolved
    /// string (harmless here because every reachable adapter overrides it).
    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        let _ = seat;
        ""
    }

    /// Adapter-owned ordering for model-change pacing hints. Larger means a
    /// stronger rung; `None` keeps unknown ids informational only.
    fn model_strength(&self, model: &str) -> Option<u8> {
        let _ = model;
        None
    }

    /// Optional worker model used when the operator has no override; `None` leaves selection to the launched harness.
    fn default_worker_model(&self) -> Option<&'static str> {
        None
    }

    /// Arguments that add `prompt` to this agent's system prompt for one run.
    /// Empty when the agent has no verified mechanism, which is how an
    /// unsupported agent ships without injection rather than with a guess.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String>;

    /// This agent's own base system prompt: text that only makes sense for
    /// this agent, because it names that agent's tools and conventions.
    /// Composed as a base layer, after the shipped default and before every
    /// layer a human wrote, so the user, repo and command-line layers all
    /// still append after it and still take precedence.
    ///
    /// `None` (the default) means this agent contributes nothing of its own,
    /// which is what an agent whose tool vocabulary zirv has not verified
    /// must do rather than be handed another agent's instructions.
    ///
    /// `posture` is this seat's own repository-write guard posture (issue
    /// #358 T8, `SuperviseConfig::orchestrator_writes`) -- the one place
    /// this layer's text depends on live config rather than being purely
    /// static, because it names the guard's actual enforcement behaviour
    /// (`prompt::orchestrator_write_lines`). Owned `String`, unlike every
    /// other layer on this trait, for exactly that reason.
    fn base_system_prompt(&self, posture: OrchestratorWrites) -> Option<String> {
        let _ = posture;
        None
    }

    /// This agent's own layer for a delegated **Worker** session -- the
    /// role-scoped counterpart to [`base_system_prompt`](Self::
    /// base_system_prompt), which is spliced in for an **Orchestrator**
    /// session only. Exactly one of the two ever reaches a launch, so a
    /// worker never receives the orchestrator layer's own delegate-and-review
    /// coaching: telling a session that was itself delegated to that its job
    /// is to delegate is what invites the recursion `zirv agent`'s workers
    /// must not do.
    ///
    /// `None` (the default) means this agent contributes no worker-specific
    /// layer of its own, the same "no verified mechanism" shape every other
    /// optional layer on this trait uses.
    fn worker_system_prompt(&self) -> Option<&'static str> {
        None
    }

    /// This agent's own layer for a `PromptRole::SubOrchestrator` session --
    /// a coordinator handed one scope, which may dispatch Workers but not
    /// spawn another coordinator (see `PromptRole::SubOrchestrator`).
    ///
    /// Defaults to the Worker layer: an adapter with nothing coordinator-
    /// specific to say should say the safer thing, not the more permissive
    /// one.
    fn sub_orchestrator_system_prompt(&self) -> Option<&'static str> {
        self.worker_system_prompt()
    }

    /// The user-facing flag name `system_prompt_args` emits, when the agent has
    /// one. Lets a caller find and merge a user's own use of the flag instead
    /// of silently overriding it with a second occurrence. `None` when the
    /// agent has no such flag, which is also the default: nothing to merge.
    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        None
    }

    /// The user-facing flag name that delivers the composed prompt via a
    /// file path instead of argv text, when this agent has a verified one.
    /// `None` (the default) means: use `system_prompt_args`, which puts the
    /// prompt on argv instead.
    fn system_prompt_file_flag(&self) -> Option<&'static str> {
        None
    }

    /// Whether the binary about to be spawned advertises
    /// `system_prompt_file_flag` in its own `--help`. Probed rather than
    /// assumed: an adapter can know a flag's name and still find it missing
    /// from an older install.
    ///
    /// `launch` is the argv the caller is about to spawn, and the probe must
    /// hit exactly that program: `wrap` spawns the user's own argv, which can
    /// be an entirely different install from the one `agent_bin` names, and
    /// handing the file flag to a binary that does not have it fails the
    /// launch outright. An empty `launch` means the adapter's own program.
    ///
    /// `false` -- the default, and the fallback for any probe failure -- means
    /// argv delivery via `system_prompt_args`, never a blocked launch.
    fn supports_system_prompt_file(&self, launch: &[String]) -> bool {
        let _ = launch;
        false
    }

    /// Whether a headless launch this adapter builds resolves to the Windows
    /// `cmd.exe /c <shim>` form (an npm-installed `.cmd`), where cmd.exe
    /// reparses the whole downstream command line. The default derives the
    /// answer from [`resolve_program`]'s own resolution of
    /// [`program()`](Self::program) -- via the free
    /// [`launches_through_cmd_shim`] function -- rather than assuming a
    /// permissive `false`: an adapter that overrides nothing is still
    /// protected, because zirv already knows whether the binary it resolved
    /// is a `.cmd`/`.bat` shim. `false` off Windows and for a directly
    /// executable program, same as before. When `true`, a caller delivers
    /// the headless prompt -- and any folded mail -- on the child's stdin via
    /// [`headless_cmd_stdin`](Self::headless_cmd_stdin) rather than as an
    /// argv token, so that untrusted free text never reaches cmd.exe's parser
    /// (`guard_cmd_shim_reparse` is only the fail-closed backstop). Override
    /// only for a deliberate, reviewable opt-*out* -- there is no legitimate
    /// reason today to opt an adapter *into* more protection than this
    /// derivation already grants it.
    fn launches_through_cmd_shim(&self) -> bool {
        launches_through_cmd_shim(self.program())
    }

    /// A headless launch that expects its prompt on **stdin** rather than as
    /// the `-p <prompt>` argv token, for the
    /// [`launches_through_cmd_shim`](Self::launches_through_cmd_shim) case.
    /// `None` (the default) means this agent has no verified stdin form, so the
    /// caller keeps argv delivery. When `Some`, the returned `Command` reads
    /// its prompt from stdin to EOF -- the same mechanism the distiller uses --
    /// and the caller must pipe the prompt in.
    fn headless_cmd_stdin(&self, session: &SessionId, extra: &[String]) -> Option<Command> {
        let _ = (session, extra);
        None
    }

    /// How many leading argv tokens are the program invocation itself rather
    /// than flags the operator passed. One for a bare binary; more when
    /// `agent_bin` carries arguments, since `"/usr/bin/env claude"` spends two
    /// tokens before the first real flag. A relaunch rebuilds the invocation
    /// from `headless_cmd`, so anything inside this prefix must never be
    /// carried over as if the operator had asked for it.
    fn launch_prefix_len(&self) -> usize {
        1
    }

    /// Issue #382: an adapter whose harness keeps its transcript as a JSON
    /// snapshot file or a SQLite database -- not naturally line-local JSONL
    /// -- materializes newly seen rows into a
    /// `super::transcript_source::ShadowTranscript` from inside this call
    /// (`sync_json_array`/`sync_sqlite`) and returns the resulting shadow
    /// path instead of the harness's own file. This is existing precedent,
    /// not a new rule: `CodexAdapter::transcript_path` already performs I/O
    /// inside this call (it scans `~/.codex/sessions` and reads/writes a pin
    /// under `StateDir::rollouts()`). When no `StateDir` resolves, the
    /// native path is returned instead (degraded, the same fallback shape
    /// codex's own pin lookup uses) -- there is no trait method for this, an
    /// adapter simply calls `StateDir::resolve` itself. Whichever path is
    /// returned is always line-local JSONL, so [`parse_events`](Self::parse_events)
    /// below stays line-local for every adapter, shadowed or not.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf;

    /// Must be line-local: every line's events depend on that line alone, so
    /// parsing a transcript in pieces cut at newlines and concatenating the
    /// results is the same as parsing the whole of it. The incremental scoring
    /// path in `score.rs` feeds each adapter only the bytes appended since the
    /// last pass, and that is what makes it equal to a full parse.
    fn parse_events(&self, jsonl: &str) -> Vec<NormalizedEvent>;
    fn structural_context(&self, jsonl: &str, last_n: usize) -> StructuralContext;

    /// Final report text only; structural extraction excludes tool arguments.
    fn final_assistant_message(&self, jsonl: &str) -> Option<String> {
        self.structural_context(jsonl, 1).assistant_texts.pop()
    }

    /// The most recently observed live model id inside `jsonl`, or `None`
    /// when this adapter has no per-transcript model signal (the default) or
    /// the fragment happens to carry none. Must be line-local, exactly like
    /// [`parse_events`](Self::parse_events): `score.rs` feeds this only the
    /// bytes appended since the last poll, so a caller keeps the last value
    /// it resolved across polls rather than treating a fragment with no hit
    /// as "no model at all".
    ///
    /// Issue #155 D1: this is what lets a live scoring path call
    /// [`capabilities_for_model`](Self::capabilities_for_model) with a real
    /// model string instead of always falling back to the conservative
    /// "unstated model" reading -- see that method's own doc comment.
    fn model_hint(&self, jsonl: &str) -> Option<String> {
        let _ = jsonl;
        None
    }

    /// The model context window this session's own transcript states, or
    /// `None` when this adapter's transcript shape carries no such figure
    /// (the default) or the fragment happens not to state one. Line-local
    /// for the same reason [`model_hint`](Self::model_hint) is, and carried
    /// across polls by the same caller.
    ///
    /// The counterpart to
    /// [`context_window_tokens`](Self::context_window_tokens), which answers
    /// from a model id alone: this answers from what the harness itself
    /// reported for THIS session, which is strictly better evidence when it
    /// exists, so `score.rs` lets it override the capability before the
    /// capacity-aware gates (`rot::token_gates`) are computed. An operator's
    /// own `score.model_context_tokens` still outranks both -- they know
    /// their seat. `rot.rs` learns nothing new: the figure arrives inside
    /// `Capabilities`, which it already receives, so the engine stays pure.
    fn context_window_hint(&self, jsonl: &str) -> Option<u64> {
        let _ = jsonl;
        None
    }

    /// Cumulative input/output usage exposed by this harness's transcript.
    /// This is deliberately separate from rot's latest-context token signal:
    /// workflow telemetry needs phase cost, not current context occupancy.
    fn transcript_usage(&self, jsonl: &str) -> Option<TranscriptUsage> {
        let _ = jsonl;
        None
    }

    /// Whether [`transcript_usage`](Self::transcript_usage) returns the
    /// transcript's cumulative latest snapshot instead of summing only the
    /// supplied JSONL fragment.
    fn transcript_usage_is_cumulative(&self) -> bool {
        false
    }

    /// Advertise tool-call events only when verified, so max-tool-calls cannot be accepted without an enforceable counter. (#155)
    fn counts_tool_calls(&self) -> bool {
        true
    }

    fn compact_command(&self) -> Option<&'static str>;
    fn quit_sequence(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;

    /// This adapter's usable context window for `model`, when it can state
    /// one. `None` -- the default -- means no verified capacity, which
    /// leaves rotation on its absolute thresholds. Never guess: an
    /// overstated capacity raises the restart ceiling past what the seat
    /// holds, and overrunning a window is worse than rotating early.
    fn context_window_tokens(&self, _model: Option<&str>) -> Option<u64> {
        None
    }

    /// [`capabilities`](Self::capabilities) with the context window resolved
    /// for a KNOWN model. Callers that have a model string to hand use this;
    /// everything else keeps calling `capabilities()`, which carries the
    /// adapter's own conservative default.
    ///
    /// Issue #155 D1: `score.rs`'s live scoring paths (`full_score`,
    /// `IncrementalScorer::poll`) resolve the model via
    /// [`model_hint`](Self::model_hint) off the transcript they already have
    /// in hand and call this instead of `capabilities()`, so a `[1m]` claude
    /// seat's real 1M window reaches rot's token gates rather than the 200k
    /// baseline every unstated model gets.
    fn capabilities_for_model(&self, model: Option<&str>) -> Capabilities {
        Capabilities {
            context_window_tokens: self.context_window_tokens(model),
            ..self.capabilities()
        }
    }

    /// A verified harness-owned way to present a local artifact directly in
    /// that harness's UI, without launching a browser or development server.
    /// Current Claude Code and Codex CLI adapters intentionally keep the
    /// default: accepting an image as model input is not the same capability
    /// as presenting an output artifact to the operator.
    fn native_artifact_presentation(
        &self,
        path: &Path,
        interactive_required: bool,
    ) -> Option<&'static str> {
        let _ = (path, interactive_required);
        None
    }

    /// Whether this concrete launch has a safe system-prompt channel. Most
    /// adapters are launch-invariant; adapters using shell shims can narrow
    /// their advertised capability for the unsafe launch shape.
    fn system_prompt_supported(&self, launch: &[String]) -> bool {
        let _ = launch;
        self.capabilities().system_prompt
    }

    /// What this harness can actually deliver for one of zirv's own policy
    /// capabilities at one requested stance -- the per-adapter half of
    /// `policy::evaluate`, which is the only caller.
    ///
    /// Answer with a `CapabilityDescriptor` naming the **verified per-run
    /// mechanism** this adapter would pin on the launch, or with
    /// `CapabilityDescriptor::advisory_only()` -- the default -- when there is
    /// none. That default is the same "no verified mechanism" shape every
    /// other optional method on this trait uses, and here it carries the
    /// load-bearing honesty rule: prompt text asking a session to respect a
    /// stance is advisory context, never enforcement, so a harness with only
    /// that to offer must report `Support::Unsupported` rather than claim a
    /// guarantee zirv cannot keep.
    ///
    /// `stance` is never `Stance::Allow`: `policy::evaluate` answers that case
    /// itself (zirv is imposing nothing, so there is no mechanism to name), so
    /// an implementation may leave it to a catch-all arm.
    ///
    /// `policy::evaluate` is `policy_support`'s only caller; `agent.rs`'s
    /// `zirv agent` headless-warning path (issue #230 item 3) is its own
    /// production caller now. Both adapters override this default, and
    /// `policy.rs`'s own tests exercise every arm.
    ///
    /// This method only ever sees one `(capability, stance)` pair at a time,
    /// so it cannot express a cross-capability implication -- e.g. claude's
    /// tool-deny pin denying all writes also happens to cover
    /// `outside_repo_fs_write`/`git_push_destructive` whenever
    /// `repo_fs_write = deny`, in the safe (narrowing) direction, but each of
    /// those still answers `Unsupported` in isolation. Issue #44, which pins
    /// a stance onto a real launch, needs the whole `EffectivePolicy` in
    /// hand to exploit an implication like that; it is not visible from this
    /// signature alone.
    fn policy_support(
        &self,
        capability: super::policy::Capability,
        stance: super::policy::Stance,
        mode: LaunchMode,
    ) -> super::policy::CapabilityDescriptor {
        let _ = (capability, stance, mode);
        super::policy::CapabilityDescriptor::advisory_only()
    }

    /// What this adapter can honestly do with a non-empty `[policy]
    /// network_allowlist` (issue #727), when `Network`'s own resolved
    /// `stance` is not `Deny` -- `policy::evaluate` calls this INSTEAD of
    /// [`policy_support`](Self::policy_support) for exactly that
    /// (capability, stance) pair, because `policy_support`'s own signature
    /// has no way to see the allowlist itself. Every other capability, and
    /// `Network` with an empty allowlist, still go through `policy_support`
    /// unchanged.
    ///
    /// Default is [`CapabilityDescriptor::advisory_only`](super::policy::
    /// CapabilityDescriptor::advisory_only), the same "no verified mechanism"
    /// answer `policy_support`'s own default gives for `Network` -- an
    /// adapter with no verified way to scope network by destination is in
    /// exactly the same honest position whether or not an allowlist was
    /// configured.
    fn network_allowlist_support(
        &self,
        allowlist: &[super::policy::NetworkTarget],
        stance: super::policy::Stance,
        mode: LaunchMode,
    ) -> super::policy::CapabilityDescriptor {
        let _ = (allowlist, stance, mode);
        super::policy::CapabilityDescriptor::advisory_only()
    }

    /// Apply resolved policy to the real launch; unsupported adapters return no flags and must report that gap honestly.
    fn policy_args(
        &self,
        policy: &super::policy::EffectivePolicy,
        mode: LaunchMode,
    ) -> Vec<String> {
        let _ = (policy, mode);
        Vec::new()
    }

    /// Default launches allow workspace work without prompts while retaining a sandbox or structural deny; unattended approval requests fail closed.
    fn default_sandbox_args(
        &self,
        sandbox: &super::config::SandboxConfig,
        safety: &super::safety::SafetyPolicy,
        network_allowlist: &[super::policy::NetworkTarget],
        mode: LaunchMode,
    ) -> Vec<String> {
        let _ = (sandbox, safety, network_allowlist, mode);
        Vec::new()
    }

    /// `default_sandbox_args` for a launch whose prompt role is known; adapters that narrow the tool surface by role override it.
    fn default_sandbox_args_for_role(
        &self,
        sandbox: &super::config::SandboxConfig,
        safety: &super::safety::SafetyPolicy,
        network_allowlist: &[super::policy::NetworkTarget],
        mode: LaunchMode,
        role: Option<super::prompt::PromptRole>,
    ) -> Vec<String> {
        let _ = role;
        self.default_sandbox_args(sandbox, safety, network_allowlist, mode)
    }

    /// Add only launch-specific worktree and zirv state roots, using the adapter's verified CLI mechanism at the actual spawn seam.
    /// Caller contract: only call for a launch that is actually happening, never
    /// speculatively. The state roots are `StateDir::workload_writable_dirs`,
    /// never the whole state root -- policy snapshots and the decision log must
    /// stay unwritable by the workload.
    fn extra_writable_root_args(&self, cwd: &Path, state: &super::state::StateDir) -> Vec<String> {
        let _ = (cwd, state);
        Vec::new()
    }

    /// Whether the host already lists the skills natively for `role` under these launch `flags` (exactly when `plugin_dir_args` attaches them), making a prompt-embedded listing a duplicate.
    fn lists_skills_natively(&self, role: super::prompt::PromptRole, flags: &[String]) -> bool {
        let _ = (role, flags);
        false
    }

    /// One orchestrator-prompt sentence routing workers to the host plugin's lean worker agent; appended only when the plugin attaches (`lists_skills_natively`).
    fn plugin_worker_routing(&self) -> Option<&'static str> {
        None
    }

    /// Register native host skills when the launch permits them; worker and single-seat prompts already carry the compact skill index.
    fn plugin_dir_args(&self, flags: &[String], role: super::prompt::PromptRole) -> Vec<String> {
        let _ = (flags, role);
        Vec::new()
    }

    fn register_turn_signal(&self, session: &SessionRef, socket: &Path) -> TurnSignalSetup;

    /// Argv tokens that select `model` for one interactive launch (the
    /// dashboard's orchestrator pane, via `chat.model`/`ZIRV_CTX_CHAT_MODEL`).
    /// Appended after the launch prefix, alongside any other `extra` argv
    /// `interactive_cmd` receives. The default is empty, matching every other
    /// "no verified mechanism" trait default on this trait
    /// (`system_prompt_args`, `base_system_prompt`): an adapter with no
    /// verified flag ships with no model selection rather than a guess.
    ///
    /// Both current adapters override this default. `chat.rs` (dashboard
    /// Task 6) now calls `model_args` through `dyn AgentAdapter` when it
    /// builds the orchestrator pane's argv.
    fn model_args(&self, model: &str) -> Vec<String> {
        let _ = model;
        Vec::new()
    }

    /// Pin roster and launch model through the same operator endpoint override. (#395)
    fn pin_model_for_endpoint(&self, model: &str) -> String {
        model.to_string()
    }

    /// Argv tokens that resume `session_id`'s own conversation, for the
    /// dashboard's quit/restore roster (`dash::roster::restore_argv`, called
    /// through `dyn AgentAdapter` -- unlike `model_args` above, both
    /// adapters reach this default body today, since codex does not
    /// override it). `None` -- the default, and every "no verified
    /// mechanism" trait default's own answer -- means this agent's resume
    /// story is unverified: a restore falls back to a fresh launch carrying
    /// a plain one-line "resuming after a dashboard restart" prompt instead
    /// of trying to guess a flag.
    fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        let _ = session_id;
        None
    }

    /// Whether this harness accepts an initial prompt *alongside* its own
    /// resume flags, so one launch can both continue an existing
    /// conversation and say something new in it. `false` -- the default --
    /// means resuming and prompting are mutually exclusive here as far as
    /// this codebase has verified, and a caller that must deliver a handoff
    /// packet has to choose the cold launch instead of the resume.
    ///
    /// This is what makes a RETURN to a parked harness possible: that
    /// conversation missed everything the interim harness did, so it may
    /// only be resumed by a launch that can also carry the interim's own
    /// handoff packet.
    fn resume_accepts_prompt(&self) -> bool {
        false
    }

    /// Issue #462: whether `session` names a conversation this harness could
    /// actually be resumed into, when that is knowable from what the harness
    /// itself has on disk. `None` -- the default -- means this adapter has no
    /// verified way to tell, and a caller must fall back to its own
    /// evidence rather than treat "unknown" as either answer.
    ///
    /// This exists because [`session_pin_args`](Self::session_pin_args) is
    /// not always applied: a launch the operator pinned themselves (`zirv
    /// chat -- --resume <id>`), or any harness with no pin flag, leaves
    /// zirv's uuid a zirv-side handle only. A recovery path that resumes it
    /// blind gets the harness's own "no conversation found" and the pane
    /// dies -- which, for the orchestrator pane, closes the dashboard's own
    /// seat.
    fn conversation_exists(&self, session: &SessionRef) -> Option<bool> {
        let _ = session;
        None
    }

    /// Argv tokens that make this agent adopt zirv's own `session` uuid as the
    /// id of the conversation it is about to start, so a later
    /// [`resume_args`](Self::resume_args) against that same uuid finds
    /// something. Empty -- the default -- means the agent mints its own
    /// conversation id and zirv's uuid is only ever a zirv-side handle.
    ///
    /// Appended **only** to a dashboard pane's launch (`chat.rs::
    /// dash_orchestrator_pane` and `dash::fulfill_spawn_request`, both of
    /// which own a freshly minted uuid), never inside `interactive_cmd`
    /// itself: `wrap`'s relaunch path deliberately lets the harness mint a
    /// fresh conversation on every restart, and a restored pane already
    /// carries `resume_args`, which would conflict with a pin.
    ///
    /// Without this, the dashboard's restore roster stored a uuid the agent
    /// had never heard of: `claude --resume <zirv-uuid>` answered "no
    /// conversation found" and the restored pane died immediately.
    fn session_pin_args(&self, session: &str) -> Vec<String> {
        let _ = session;
        Vec::new()
    }

    /// Issue #418: this agent's own native, user-level hooks configuration
    /// target -- the file zirv should install/inspect/remove its own
    /// `PreToolUse`/`PostToolUse`-equivalent entries into, and the entries
    /// themselves. `None` -- the default -- means no verified native hooks
    /// surface exists at all (codex, opencode, pi, qwen); `zirv ctx hook
    /// install <agent>` refuses cleanly on it. `home` is the bare home
    /// directory (`crate::utils::home_dir()`'s own answer, or a tempdir in a
    /// test); an implementation resolves its own subdirectory from it, the
    /// same way `DroidAdapter::home_dir`/`GeminiAdapter::home_dir` already
    /// join `.factory`/`.gemini` onto their own resolved home.
    fn native_hooks(&self, home: &Path) -> Option<super::native_hooks::NativeHooks> {
        let _ = home;
        None
    }
}

/// An adapter constructor: the same shape `ClaudeAdapter::new` and
/// `CodexAdapter::new` already share, named so `ADAPTERS` reads as a table
/// rather than a wall of type punctuation.
pub type AdapterCtor = fn(Option<&str>) -> Box<dyn AgentAdapter>;

fn make_claude(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(claude::ClaudeAdapter::new(bin))
}

fn make_codex(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(codex::CodexAdapter::new(bin))
}

fn make_copilot(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(copilot::CopilotAdapter::new(bin))
}

fn make_cursor(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(cursor::CursorAdapter::new(bin))
}

fn make_droid(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(droid::DroidAdapter::new(bin))
}

fn make_gemini(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(gemini::GeminiAdapter::new(bin))
}

fn make_goose(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(goose::GooseAdapter::new(bin))
}

fn make_grok(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(grok::GrokAdapter::new(bin))
}

fn make_kimi(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(kimi::KimiAdapter::new(bin))
}

fn make_muse(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(muse::MuseAdapter::new(bin))
}

fn make_opencode(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(opencode::OpenCodeAdapter::new(bin))
}

fn make_pi(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(pi::PiAdapter::new(bin))
}

fn make_qwen(bin: Option<&str>) -> Box<dyn AgentAdapter> {
    Box::new(qwen::QwenAdapter::new(bin))
}

/// The single source of truth for which adapters exist: a name paired with a
/// constructor. Adding an adapter is one entry here (plus its own module) --
/// `all`, `select`'s fallback, `describe_known_adapters`, `resolve_default`
/// and `readiness_note` all walk this table rather than naming adapters by
/// hand, so none of them can drift from it.
pub const ADAPTERS: &[(&str, AdapterCtor)] = &[
    ("claude", make_claude),
    ("codex", make_codex),
    ("copilot", make_copilot),
    ("cursor-agent", make_cursor),
    ("droid", make_droid),
    ("gemini", make_gemini),
    ("goose", make_goose),
    ("grok", make_grok),
    ("kimi", make_kimi),
    ("muse", make_muse),
    ("opencode", make_opencode),
    ("pi", make_pi),
    ("qwen", make_qwen),
];

pub fn all(bin: Option<&str>) -> Vec<Box<dyn AgentAdapter>> {
    ADAPTERS.iter().map(|(_, ctor)| ctor(bin)).collect()
}

/// Issue #395: attaches `cfg.endpoint.claude`/`cfg.endpoint.codex` (whichever
/// names `adapter`, if either) via [`AgentAdapter::apply_endpoint`]. The one
/// helper `select`/`resolve_default` call right after constructing an
/// adapter and before `adapter.ready()`, so a configured endpoint's
/// credential check rides the same pre-spawn gate a genuinely unlaunchable
/// binary already goes through.
fn apply_endpoint_override(adapter: &mut Box<dyn AgentAdapter>, cfg: &CtxConfig) {
    let target = match adapter.name() {
        "claude" => cfg.endpoint.claude.as_ref(),
        "codex" => cfg.endpoint.codex.as_ref(),
        _ => None,
    };
    adapter.apply_endpoint(target);
}

/// Issue #504: attaches `cfg.chat` (via [`AgentAdapter::apply_chat_config`])
/// the same way [`apply_endpoint_override`] attaches `cfg.endpoint` --
/// called at each of that function's own call sites, right after
/// constructing the adapter. Whole-`ChatConfig` rather than a single
/// resolved field: `apply_endpoint_override` picks per-adapter because
/// `[endpoint.claude]`/`[endpoint.codex]` are two different tables, but
/// `[chat]` has no per-adapter split, so every adapter's own `apply_chat_
/// config` reads the one shared config directly.
fn apply_chat_override(adapter: &mut Box<dyn AgentAdapter>, cfg: &CtxConfig) {
    adapter.apply_chat_config(&cfg.chat);
    adapter.apply_approvals_config(&cfg.approvals);
}

/// Issue #788: attaches `cfg.headless` (via [`AgentAdapter::
/// apply_headless_config`]) the same way [`apply_chat_override`] attaches
/// `cfg.chat` immediately above -- called at each of that function's own
/// call sites, right after constructing the adapter.
fn apply_headless_override(adapter: &mut Box<dyn AgentAdapter>, cfg: &CtxConfig) {
    adapter.apply_headless_config(&cfg.headless);
}

/// Issue #395: the credential-presence check both `ClaudeAdapter::ready`
/// and `CodexAdapter::ready` apply when an operator endpoint override is
/// configured. Named by the environment variable's own NAME only, never its
/// value -- a missing/empty key fails loudly here, before any child is
/// spawned, rather than launching a request with no auth token at all.
pub(super) fn require_endpoint_credential(target: &super::config::EndpointTarget) -> CtxResult<()> {
    match std::env::var(&target.credential_env) {
        Ok(value) if !value.is_empty() => Ok(()),
        _ => Err(format!(
            "endpoint credential environment variable `{}` is not set (or is empty); export it \
             before launching this agent against its configured endpoint",
            target.credential_env
        )
        .into()),
    }
}

/// Issue #89: a one-time `zirv ▸` announcement naming a resolved distiller/
/// reviewer adapter's own recorded sandbox residual
/// ([`AgentAdapter::sandbox_residual_note`]), fired at most once per
/// process. Self-contained -- builds its own [`super::announce::Announcer`]
/// rather than requiring every call site to carry one of its own, the same
/// shape `poll::announce_keychain_prompt_once` uses for the identical "no
/// per-call state to carry a latch in" reason. A no-op whenever
/// `sandbox_residual_note` is `None` (claude today, and codex once its own
/// installed version supports `--ignore-rules --ignore-user-config` -- see
/// `CodexAdapter::sandbox_residual_note`).
///
/// `chrome_events_enabled` mirrors `cfg.chrome.events` -- the same "opt-outs
/// collapse to one boolean" contract every other `zirv ▸` line honors
/// (`--quiet`/`ZIRV_CTX_QUIET`/`[chrome] events = false`) -- passed by
/// production call sites that already have a resolved `CtxConfig` in scope
/// (`handoff::run_model`'s own callers, each individually, since `run_model`
/// itself is reused by claude and codex alike and must not assume which).
/// [`read_only_args_for_agent_name`] above has no `CtxConfig` in hand at
/// all and defaults to `true`, a recorded, narrow residual: an operator
/// whose only opt-out is `--quiet`/`ZIRV_CTX_QUIET` still sees this one
/// announcement on the workflow-reviewer path specifically. See Known
/// Issues.
pub fn announce_sandbox_residual_once(adapter: &dyn AgentAdapter, chrome_events_enabled: bool) {
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(note) = adapter.sandbox_residual_note() else {
        return;
    };
    if !claim_once(&ANNOUNCED) {
        return;
    }
    super::announce::Announcer::new(chrome_events_enabled, console::colors_enabled_stderr())
        .emit(&super::announce::Event::SandboxResidual { note });
}

/// `true` the first time this specific latch flips from `false` to `true`,
/// `false` on every call after (including a concurrent caller: only one
/// `compare_exchange` wins). Extracted as its own pure function so the
/// "fires at most once" property is unit-testable against a caller-owned
/// `AtomicBool`, without needing to reset the process-wide static
/// `announce_sandbox_residual_once` actually uses between test runs (which
/// share one process and would otherwise contaminate each other -- the same
/// reason `poll::announce_keychain_prompt_once`/`config::announce_
/// unparsable_layers_once` have no dedicated "fires once" test of their
/// own today).
fn claim_once(latch: &std::sync::atomic::AtomicBool) -> bool {
    latch
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

/// Static adapter lookup for [`AgentAdapter::native_artifact_presentation`].
/// This does not require an installed/ready harness: presentation support is
/// adapter metadata, while the caller separately applies enablement and
/// canonical policy.
pub fn native_artifact_presentation_for_agent_name(
    name: &str,
    path: &Path,
    interactive_required: bool,
) -> Option<&'static str> {
    ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .and_then(|(_, ctor)| ctor(None).native_artifact_presentation(path, interactive_required))
}

/// `cfg.agent_bin` is one global override applied to *whichever* adapter is
/// selected (every `ctor(bin)` call in this module reuses the same value
/// regardless of the adapter name) -- there is no per-adapter binary
/// override. That is fine for a stub path, a wrapper script (`sh
/// /path/fake-codex-agent.sh`), or a differently located install of the
/// *same* agent, but a value whose own program basename names a *different*
/// registered adapter (`agent_bin = "/usr/local/bin/claude"` while `codex`
/// is what gets selected, most plausibly stale config left over from
/// switching agents) would launch that other agent's real binary dressed up
/// in the selected adapter's own argv shape -- codex's `exec <prompt>`
/// positional form handed to the real claude CLI, wrong account, wrong
/// safety model, and no error anywhere naming what happened. Checked by
/// basename only (extension stripped, case-insensitive), not full-path
/// identity: an operator who genuinely renamed a binary to something that
/// happens to collide with another adapter's own name gets the same
/// refusal, which is the conservative, name-the-problem-and-stop failure
/// mode this guard exists for.
///
/// Returns the *other* adapter's name when `bin`'s basename collides with
/// one that is not `selected`; `None` when `bin` is unset, names no
/// registered adapter at all (a stub/wrapper path, the common test and
/// wrapper-script shape), or names `selected` itself.
fn agent_bin_names_a_different_adapter(bin: Option<&str>, selected: &str) -> Option<&'static str> {
    let bin = bin?;
    let program = bin.split_whitespace().next()?;
    let stem = Path::new(program).file_stem()?.to_str()?;
    ADAPTERS.iter().find_map(|(name, _)| {
        (!name.eq_ignore_ascii_case(selected) && stem.eq_ignore_ascii_case(name)).then_some(*name)
    })
}

/// The clear, name-both-adapters refusal `agent_bin_names_a_different_
/// adapter` backs, shared by every `select`/`resolve_default` arm that is
/// about to return `selected` as the resolved adapter.
fn refuse_if_agent_bin_names_another_adapter(bin: Option<&str>, selected: &str) -> CtxResult<()> {
    if let Some(other) = agent_bin_names_a_different_adapter(bin, selected) {
        return Err(format!(
            "agent_bin '{}' names '{other}', not the selected agent '{selected}' -- refusing to \
             launch '{other}'s binary as if it were '{selected}'. Point agent_bin at a '{selected}' \
             install, or select '{other}' instead.",
            bin.unwrap_or_default()
        )
        .into());
    }
    Ok(())
}

/// The registry's names, each suffixed `(disabled)` when `gate` refuses it --
/// used by the unknown-name error so a mistyped `--agent` also shows which
/// known names are actually usable right now.
fn describe_known_adapters(gate: &crate::settings::AgentGate) -> String {
    ADAPTERS
        .iter()
        .map(|(name, _)| {
            if gate.is_enabled(name) {
                name.to_string()
            } else {
                format!("{name} (disabled)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// List enabled, ready adapters for launch errors, in registry order.
pub fn available_adapter_names(cfg: &CtxConfig) -> Vec<&'static str> {
    let bin = cfg.agent_bin.as_deref();
    ADAPTERS
        .iter()
        .filter(|(name, ctor)| cfg.agents.is_enabled(name) && ctor(bin).ready().is_ok())
        .map(|(name, _)| *name)
        .collect()
}

/// Which rule picked the default adapter, for callers (`zirv ctx status`,
/// diagnostics) that want to explain the choice rather than just use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultOrigin {
    /// `cfg.agent` named it explicitly.
    Configured,
    /// No configured agent; this was the first adapter in registry order
    /// that was both gate-enabled and `ready()`.
    FirstEnabledReady,
    /// As `FirstEnabledReady`, except that *presence* rather than registry
    /// order decided it: at least one candidate ahead of it was dropped for
    /// being confidently absent from this machine, and `not_found` names the
    /// first such one -- the adapter this fallback would have landed on had
    /// it been installed. Issue #690: a caller that reports the choice at
    /// all must report this one, which is what keeps "the only harness you
    /// actually have" from being a silent provider switch. See
    /// [`resolve_default`]'s own `G3` note.
    FirstInstalledReady { not_found: &'static str },
}

/// Resolves the adapter `select` falls back to when neither an explicit
/// `--agent` nor detection named one: `cfg.agent` if set, else the first
/// registry entry that is both gate-enabled and `ready()`. Every call site
/// of `select` already folds `cfg.agent` into the `name` it passes in, so by
/// the time `select`'s fallback arm calls this, `cfg.agent` is always `None`
/// there -- but this function stands on its own (and is tested that way),
/// since a `None` name is not the only way to reach "use the configured or
/// default agent".
///
/// When nothing qualifies, the error aggregates one line per adapter naming
/// why it was skipped, reusing the gate's own refusal text and each
/// adapter's own `ready()` text rather than inventing new wording.
///
/// G: a repo checkout's own `.settings.toml` may narrow this fallback (take
/// an adapter off the table) but must never *select* a different one for the
/// operator as a side effect of that narrowing -- a repo-only disable
/// (`AgentGate::disabled_only_by_repo`) that would otherwise leave the
/// fallback silently landing on a different, still-enabled adapter refuses
/// instead, naming both adapters and the fix. Skipping past a repo-disabled
/// adapter when *nothing else* qualifies either is unaffected: no different
/// provider was ever silently chosen, so the ordinary aggregate error still
/// applies and still names every candidate.
///
/// G2 (fix): `repo_narrowed` is only recorded when the repo-disabled adapter
/// would *also* have passed `ready()` -- otherwise it was never a candidate
/// this fallback could have landed on in the first place (an unlaunchable
/// bare name, say), and the refusal's own claim that it "would otherwise
/// have been the default agent" would be false. Without this, disabling an
/// already-unlaunchable adapter via `.settings.toml` could still block a
/// perfectly good fallback to the next one, over a hypothetical that was
/// never true.
///
/// G3 (issue #690): the fallback arm -- and only that arm -- also drops a
/// candidate whose program is *confidently* absent from this machine. A repo
/// checkout narrowing the fallback and a harness simply not being installed
/// are not the same act by the same actor: the first is an untrusted surface
/// changing which vendor an operator pays, which G above still refuses; the
/// second is a fact about the operator's own machine, the same trust tier as
/// `~/.zirv/ctx.toml`, and refusing there helps nobody -- an operator whose
/// only harness is codex, with no `agent` configured, otherwise gets
/// claude's "program 'claude' not found" and no way forward. What was
/// legitimate in the objection is the word *silent*, so the answer is to
/// announce rather than refuse: landing on a later adapter because an
/// earlier one is not installed returns `DefaultOrigin::FirstInstalledReady`,
/// naming the missing one, and every surface that reports the rule at all
/// reports that.
///
/// Fail-open, exactly as [`Liveness`] and [`program_is_present`] already
/// require: only `Absent` drops a candidate. `Live` and `Unknown` both keep
/// it, so a probe that cannot reach a verdict leaves this function behaving
/// precisely as it did before presence was consulted at all.
///
/// G3 and the probe cache: selection probes fresh, every launch.
/// [`ProbeCache`] exists for the injected roster, which re-renders on every
/// compile and can afford an hour-stale verdict; a launch path cannot. A
/// cached `Absent` that outlives the install it describes would keep routing
/// an operator's work to another vendor for the rest of
/// `PROBE_CACHE_TTL_SECS` after they installed the harness they actually
/// want -- and the operator who has just installed one is exactly who this
/// rule exists for. The cost is a few dozen `stat`s once per launch. The
/// price is that this fallback and the roster can briefly disagree about one
/// adapter, which is the right way round: the roster only annotates a
/// prompt, this decides whose account gets spent.
///
/// G3 and `agent_bin`: presence answers "is this adapter's *own* program
/// installed", which is not a question about a program the operator has
/// already named. An `agent_bin` override need not even be a path a `stat`
/// can answer -- the `sh <wrapper>.sh` shape this codebase's own fixtures
/// use throughout resolves to nothing on disk, and [`program_is_present`]'s
/// own doc comment records that several call sites depend on
/// `resolve_program` failing open for exactly such a value. So while an
/// override is in effect, presence is not consulted at all: pointing zirv
/// at a binary is the same kind of act as naming the agent, an explicit
/// operator choice this rule exists to inform rather than to overrule.
///
/// G3 and G: presence is folded into G2's existing `repo_narrowed`
/// condition, not checked beside it. A repo-disabled adapter that is itself
/// confidently absent was never a candidate this fallback could have landed
/// on -- the same false premise G2 already guards against, reached by a
/// different route -- so it records no refusal, and the next adapter is
/// chosen and announced instead. The refusal itself still runs *before* the
/// chosen candidate's own presence check: when the repo narrowed a harness
/// the operator really has, that refusal is the more useful answer, and G
/// must not regress into a silent switch just because the adapter it would
/// have named is missing too.
/// G3, last: this is the *launch* question ("which harness should zirv
/// start"), so presence has a say here. The naming question ("which adapter
/// is this configuration about") is [`select_for_identity`], where it has
/// none.
pub fn resolve_default(cfg: &CtxConfig) -> CtxResult<(Box<dyn AgentAdapter>, DefaultOrigin)> {
    resolve_default_with_presence(cfg, &liveness_probe)
}

/// [`resolve_default`] with its one machine-dependent input -- whether an
/// adapter's program is actually installed -- passed in rather than read
/// from the ambient `PATH`. Every test of the fallback states the machine it
/// assumes, instead of passing on the developer's laptop (claude and codex
/// both installed) and failing on a runner with neither.
pub(crate) fn resolve_default_with_presence(
    cfg: &CtxConfig,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> CtxResult<(Box<dyn AgentAdapter>, DefaultOrigin)> {
    let bin = cfg.agent_bin.as_deref();

    if let Some(name) = cfg.agent.as_deref() {
        let mut adapter = ADAPTERS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, ctor)| ctor(bin))
            .ok_or_else(|| {
                format!(
                    "unknown agent '{name}'; known adapters: {}",
                    describe_known_adapters(&cfg.agents)
                )
            })?;
        apply_endpoint_override(&mut adapter, cfg);
        apply_chat_override(&mut adapter, cfg);
        apply_headless_override(&mut adapter, cfg);
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok((adapter, DefaultOrigin::Configured));
    }

    // G3: whether presence has anything to say about this fallback at all
    // -- see this function's own doc comment on `agent_bin`.
    let consult_presence = bin.is_none();
    let mut reasons = Vec::new();
    let mut repo_narrowed: Option<&str> = None;
    // G3: the first candidate presence alone took off the table (the one
    // `FirstInstalledReady` names), and every enabled-and-ready one it did,
    // for the aggregate error's own install line.
    let mut presence_skipped: Option<&'static str> = None;
    let mut not_installed: Vec<&str> = Vec::new();
    for (name, ctor) in ADAPTERS {
        let mut adapter = ctor(bin);
        apply_endpoint_override(&mut adapter, cfg);
        apply_chat_override(&mut adapter, cfg);
        apply_headless_override(&mut adapter, cfg);
        if let Some(refusal) = cfg.agents.refusal(name) {
            // Final wave item 3: the same cross-adapter skip Medium 2 gave
            // the enabled-and-ready arm below, applied here too. Without
            // it, `ctor(bin)` on this line always builds the candidate
            // with the *global* `agent_bin`, even when `agent_bin` names a
            // different adapter entirely -- so `adapter.ready()` could
            // report "ready" for, say, a claude adapter whose `program` is
            // actually pointed at a real codex binary. That is not a
            // candidate this fallback could ever have genuinely landed on
            // (the cross-adapter guard would refuse it exactly the way
            // Medium 2 does below), so recording `repo_narrowed` from it
            // would refuse on a false premise: "claude would otherwise
            // have been the default agent" when `agent_bin` never actually
            // named claude's own binary at all.
            if repo_narrowed.is_none()
                && cfg.agents.disabled_only_by_repo(name)
                && agent_bin_names_a_different_adapter(bin, name).is_none()
                && adapter.ready().is_ok()
            {
                // G3: the last condition G2 was missing. A repo-disabled
                // adapter that is not installed at all is the same false
                // premise by another route -- it could not have been this
                // fallback's answer either way -- so it refuses nothing and
                // is announced as the missing one instead. `None` here is
                // an `agent_bin` override in effect, which presence never
                // speaks to: the refusal then stands exactly as it did.
                match consult_presence.then(|| present(name, adapter.program())) {
                    Some(Liveness::Absent(_)) => {
                        if presence_skipped.is_none() {
                            presence_skipped = Some(name);
                        }
                    }
                    _ => repo_narrowed = Some(name),
                }
            }
            reasons.push(format!("{name}: {refusal}"));
            continue;
        }
        match adapter.ready() {
            Ok(()) => {
                if let Some(narrowed) = repo_narrowed {
                    return Err(format!(
                        "the repository checkout disabled '{narrowed}' via .settings.toml, \
                         which would otherwise have been the default agent; a repo may narrow \
                         this fallback but not choose '{name}' for you instead. Pass --agent \
                         explicitly, or set `agent` in your own operator config or environment, \
                         to pick one."
                    )
                    .into());
                }
                // Keep scanning when a global binary override names another adapter; this candidate alone is not the fallback decision.
                if let Some(other) = agent_bin_names_a_different_adapter(bin, name) {
                    reasons.push(format!("{name}: agent_bin names '{other}', not '{name}'"));
                    continue;
                }
                // G3, last of all: the only check here that touches the
                // filesystem, so it runs once every cheaper reason to skip
                // this candidate has already been ruled out -- and not at
                // all while an `agent_bin` override names the program.
                if let Some(Liveness::Absent(reason)) =
                    consult_presence.then(|| present(name, adapter.program()))
                {
                    reasons.push(format!("{name}: not installed ({reason})"));
                    not_installed.push(name);
                    if presence_skipped.is_none() {
                        presence_skipped = Some(name);
                    }
                    continue;
                }
                return Ok((
                    adapter,
                    match presence_skipped {
                        Some(not_found) => DefaultOrigin::FirstInstalledReady { not_found },
                        None => DefaultOrigin::FirstEnabledReady,
                    },
                ));
            }
            Err(e) => reasons.push(format!("{name}: {e}")),
        }
    }
    let mut message = format!(
        "no agent is both enabled and ready:\n{}",
        reasons.join("\n")
    );
    if !not_installed.is_empty() {
        // Claim no harness is installed only when every candidate is confidently absent; disabled or uncertain candidates prevent that claim.
        let every_candidate = not_installed.len() == reasons.len();
        let missing = not_installed.join(", ");
        message.push_str(&format!(
            "\n{} Install one so its program is on PATH, or point `agent_bin` at it in \
             ~/.zirv/ctx.toml, or name an installed one with --agent.",
            if every_candidate {
                format!("no harness is installed on this machine (looked for: {missing}).")
            } else {
                format!("not installed on this machine: {missing}.")
            }
        ));
    }
    Err(message.into())
}

/// Explicit `--agent` name, else detection from the wrapped argv, else
/// `resolve_default`. The `.settings.toml` gate (`cfg.agents`) is checked
/// before `ready()` in every arm: `ready()` reports implementation state,
/// the gate reports operator policy, and a disabled agent must report the
/// disable rather than (for codex) "not implemented yet".
///
/// This is the entry point for a caller about to *launch* what it selects,
/// and its fallback arm may therefore drop a harness that is not installed
/// (issue #690). A caller that only needs to name an adapter -- to parse a
/// transcript, attribute usage, read capabilities -- wants
/// [`select_for_identity`] instead, where absence has no say.
///
/// It reads `command` the way `wrap` does ([`adapter_builds_launch`]): the
/// argv IS the program about to be spawned. A caller for which that is not
/// true -- `exec`, which appends a flags-only `-- --model x` to
/// `adapter.program()` -- says so for itself through
/// [`select_with_presence`] rather than taking this derivation.
pub fn select(
    name: Option<&str>,
    command: &[String],
    cfg: &CtxConfig,
) -> CtxResult<Box<dyn AgentAdapter>> {
    select_with_presence(
        name,
        command,
        cfg,
        adapter_builds_launch(command),
        &liveness_probe,
    )
}

/// What [`select`] and [`select_for_identity`] answer
/// [`select_with_presence`]'s `adapter_builds_launch` question with, on
/// behalf of a caller whose `command` IS the program about to be spawned --
/// `wrap` above all (`wrap.rs`'s `adapters::select(agent_name,
/// &args.command, &cfg)`, where `wrap -- --foo` really would try to spawn
/// `--foo`). For such a caller a non-empty `command` is the operator's own
/// argv, so zirv is not choosing a harness at all, and only an empty one
/// leaves the launch to `adapter.program()`.
///
/// Deliberately NOT widened to match `exec`'s own, looser notion (a
/// flags-only `-- --model x` is adapter-built there, because `exec` appends
/// those flags to `adapter.program()` rather than spawning them). Widened
/// here, `wrap -- --foo` would claim to be choosing a harness while it is
/// in fact about to spawn `--foo` itself, so an absent default harness
/// would refuse the operator's own argv -- the one thing `wrap` may never
/// do. `exec` states its own answer at the call site instead; see
/// [`select_with_presence`].
fn adapter_builds_launch(command: &[String]) -> bool {
    command.is_empty()
}

/// The oracle for a caller that is not choosing a harness to launch. It
/// reaches no verdict, ever, which is fail-open by [`Liveness`]'s own rule
/// and so reproduces the pre-#690 answer exactly: gate, `ready()`, registry
/// order, nothing else.
fn presence_not_consulted(_adapter_name: &str, _program: &str) -> Liveness {
    Liveness::Unknown("presence is not consulted when naming an adapter".to_string())
}

/// [`select`] for a caller that only needs to *name* the adapter this
/// configuration is about -- whose transcript format to parse, whose
/// provider a usage readout belongs to, which capabilities to assume --
/// rather than one about to launch a harness zirv itself chose.
///
/// Issue #690 gates the *choice of what to launch* on whether it is
/// installed, which is right: picking a vendor for an operator whose machine
/// leaves only one candidate is the whole point. It is wrong for every other
/// question. A transcript written by claude is claude's whether or not
/// `claude` is on this process's `PATH`, and a Stop hook subprocess
/// routinely inherits a reduced one -- gating there would silently switch
/// off screening, scoring and usage attribution on exactly the machines that
/// still have the harness, just not where a bare `PATH` walk can see it.
/// These callers spawn nothing, so absence costs them nothing and must not
/// be allowed to refuse them.
pub fn select_for_identity(
    name: Option<&str>,
    command: &[String],
    cfg: &CtxConfig,
) -> CtxResult<Box<dyn AgentAdapter>> {
    select_with_presence(
        name,
        command,
        cfg,
        adapter_builds_launch(command),
        &presence_not_consulted,
    )
}

/// [`select`] with [`resolve_default_with_presence`]'s own injected presence
/// oracle threaded through it. Only the last arm consults it at all -- an
/// explicitly named or argv-detected harness that is missing still produces
/// exactly the error it always did, never a switch to another vendor -- but
/// the seam belongs here rather than only on `resolve_default`, because the
/// invariants that matter most (a repo may narrow the fallback but never
/// choose for you) are pinned through this public entry point, and a test
/// that reached past it to `resolve_default` would no longer be testing what
/// it claims to.
///
/// `adapter_builds_launch` is the caller's own statement of whether it is
/// *choosing a harness to launch* -- whether the program it is about to
/// spawn will be `adapter.program()`. It is stated rather than derived from
/// `command` here because `command` means different things to different
/// callers, and no single derivation is right for all of them: for `wrap`
/// the command IS the program to spawn, so `wrap -- --foo` is the
/// operator's own argv; for `exec` a flags-only `-- --model x` is
/// adapter-built, because `exec` builds the launch from `adapter.program()`
/// and appends those flags to it. [`select`] and [`select_for_identity`]
/// answer it with [`adapter_builds_launch`] on their callers' behalf, which
/// is `wrap`'s reading; `exec` passes its own.
pub(crate) fn select_with_presence(
    name: Option<&str>,
    command: &[String],
    cfg: &CtxConfig,
    adapter_builds_launch: bool,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> CtxResult<Box<dyn AgentAdapter>> {
    let bin = cfg.agent_bin.as_deref();
    let adapters = all(bin);

    if let Some(name) = name {
        let found = adapters.into_iter().find(|a| a.name() == name);
        let mut adapter = found.ok_or_else(|| {
            format!(
                "unknown agent '{name}'; known adapters: {}",
                describe_known_adapters(&cfg.agents)
            )
        })?;
        apply_endpoint_override(&mut adapter, cfg);
        apply_chat_override(&mut adapter, cfg);
        apply_headless_override(&mut adapter, cfg);
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok(adapter);
    }

    if let Some(mut adapter) = adapters.into_iter().find(|a| a.detect(command)) {
        apply_endpoint_override(&mut adapter, cfg);
        apply_chat_override(&mut adapter, cfg);
        apply_headless_override(&mut adapter, cfg);
        if let Some(refusal) = cfg.agents.refusal(adapter.name()) {
            return Err(refusal.into());
        }
        adapter.ready()?;
        refuse_if_agent_bin_names_another_adapter(bin, adapter.name())?;
        return Ok(adapter);
    }

    // Only harness selection probes presence; passthrough must not refuse an operator command due to a missing default harness.
    let present: &dyn Fn(&str, &str) -> Liveness = if adapter_builds_launch {
        present
    } else {
        &presence_not_consulted
    };
    resolve_default_with_presence(cfg, present).map(|(adapter, _origin)| adapter)
}

/// True when the wrapped command can be trusted to actually be this adapter's
/// agent: either the operator named it explicitly (`--agent`, or the config's
/// `agent` key), or detection matched the command's own argv. Neither true
/// means `select`'s last arm defaulted here with nothing to back it up (an
/// arbitrary wrapped command that matches no adapter), and injecting this
/// adapter's own flags (e.g. `--append-system-prompt`) into whatever program
/// that turns out to be would leak them into its output instead of an agent
/// that would ever read them.
pub fn command_matches_adapter(
    adapter: &dyn AgentAdapter,
    agent_explicit: bool,
    command: &[String],
) -> bool {
    agent_explicit || adapter.detect(command)
}

/// The canonicalised git "common dir" that owns `path` -- the shared `.git`
/// directory a plain repo and every `git worktree add`-linked sibling of it
/// all point back at -- or `None` if `git` is missing, `path` is not inside a
/// git working tree, or the process exits non-zero. Best-effort and
/// shell-out only, same precedent as `compile::changed_repo_paths`.
///
/// `git rev-parse --git-common-dir` prints a path RELATIVE to `path` for a
/// main worktree (typically just `.git`) but an ABSOLUTE one for a linked
/// worktree (it points back at the main checkout's `.git`). Both forms are
/// resolved against `path` before canonicalising, so a main worktree and any
/// of its linked siblings canonicalise to the exact same `PathBuf` even
/// though git reports the two differently.
///
/// Code review (issue #119, round 2): this used to back an authorization
/// check in `dash/mod.rs` alone (its answer decided whether a spawn request
/// got to run a real agent), so it must not trust an inherited environment
/// that a request's own process could have set. `GIT_DIR`/`GIT_COMMON_DIR`/
/// `GIT_WORK_TREE` (and `GIT_INDEX_FILE`, for the same family of override)
/// all redirect where `git` looks for repo state regardless of `-C`'s
/// argument; left inherited, any one of them set in the calling process
/// would make `git` resolve to the SAME overridden value for two genuinely
/// unrelated paths. Stripped here, at the one seam that shells out to `git`
/// for this decision, rather than trusted to already be absent from the
/// caller's environment -- a property every caller inherits for free,
/// including `CodexAdapter::extra_writable_root_args` below.
///
/// Moved here from `dash/mod.rs` (2026-08-26, codex approval-posture round):
/// `dash` already imports `adapters` (`policy_launch_args`, `LaunchMode`,
/// ...), so `adapters` calling back into `dash` would be a cycle -- this is
/// the shared home the one-directional import graph demands, used by
/// `dash::accepted_spawn_cwd`'s eligibility check (issue #119) and by
/// `CodexAdapter::extra_writable_root_args`'s writable-root computation
/// (same issue, the other half: eligibility says a linked worktree pane may
/// run, this says its shared git dir must actually be writable once it does).
pub(crate) fn git_common_dir(path: &Path) -> Option<PathBuf> {
    git_dirs(path).map(|(_, common_dir)| common_dir)
}

/// The canonicalised working-tree git dir and shared common dir, resolved
/// in one git invocation with the same environment isolation as `git_common_dir`.
/// An unresolvable own gitdir falls back to the common dir, preserving common-dir lookup.
pub(crate) fn git_dirs(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = std::process::Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--git-dir")
        .arg("--git-common-dir")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let mut lines = raw.lines();
    let resolve = |raw: &str| {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        std::fs::canonicalize(path.join(raw)).ok()
    };
    let git_dir = resolve(lines.next()?);
    let common_dir = resolve(lines.next()?)?;
    Some((git_dir.unwrap_or_else(|| common_dir.clone()), common_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A permissive `CtxConfig` (every agent enabled, no `agent_bin`
    /// override) for tests that only care about selection, not gating.
    /// `CtxConfig::default()` never touches the filesystem or `HOME` (its
    /// `AgentGate` is `AgentGate::default()`, not a `load`), so this one
    /// needs no `HomeGuard`, unlike `cfg_disabling` below.
    pub(super) fn permissive_cfg() -> CtxConfig {
        CtxConfig::default()
    }

    /// A `CtxConfig` whose gate disables exactly one named agent, as if an
    /// operator or repo `.settings.toml` had set `[agents.<name>] enabled =
    /// false`, but without touching any file: `AgentGate`'s fields are
    /// crate-private, so the state is built by loading a real settings file
    /// from an isolated repo dir instead. `AgentGate::load` also reads the
    /// operator (home) layer, so this isolates `HOME`/`USERPROFILE` too --
    /// otherwise a developer machine's real `~/.zirv/.settings.toml` (if any)
    /// would leak into the loaded gate.
    pub(super) fn cfg_disabling(name: &str) -> CtxConfig {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            format!("[agents.{name}]\nenabled = false\n"),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        }
    }

    /// Byte offset of `needle` as a contiguous run inside `haystack`, or
    /// `None` if it never occurs (or is empty). Used to locate one building
    /// block's argv inside the assembled command without hardcoding any
    /// vendor-specific flag spelling.
    fn find_subsequence(haystack: &[String], needle: &[String]) -> Option<usize> {
        if needle.is_empty() || needle.len() > haystack.len() {
            return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Built-in read-only seats must launch with one restrictive sandbox,
    /// including when the repository policy already denies writes.
    #[test]
    fn dispatch_agent_codex_read_only_seats_have_one_sandbox() {
        use crate::commands::workflow::agents::{AgentRegistry, AgentTask};

        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let registry = AgentRegistry::load(repo.path(), None, false, false).expect("registry");
        let adapter = codex::CodexAdapter::new(None)
            .with_ignore_flags_forced(true)
            .with_exec_ask_for_approval_forced(true);
        let task = AgentTask {
            prompt: "inspect the change".to_string(),
            repo: repo.path().to_path_buf(),
            model: None,
        };
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        for policy in ["", "[policy]\nrepo_fs_write = \"deny\"\n"] {
            std::fs::write(repo.path().join(".zirv/ctx.toml"), policy).expect("config");
            for id in ["reviewer", "security-scanner", "explorer"] {
                let seat = registry
                    .list()
                    .find(|seat| seat.manifest.id == id)
                    .expect("built-in seat");
                let argv = flatten_command(
                    adapter
                        .dispatch_agent(&seat.manifest, &task)
                        .expect("dispatch"),
                );
                let sandbox: Vec<_> = argv
                    .windows(2)
                    .filter(|w| w[0] == "--sandbox")
                    .map(|w| w[1].as_str())
                    .collect();
                assert_eq!(sandbox, ["read-only"], "{id}: {argv:?}");
                assert_eq!(
                    argv.iter()
                        .filter(|arg| *arg == "--ask-for-approval")
                        .count(),
                    1,
                    "{id}: {argv:?}"
                );
            }
        }
    }

    /// Spec Risks section (issue #187, `2026-08-28-ai-native-sdlc-design.md`)
    /// calls for a cross-adapter conformance test on `AgentAdapter::
    /// dispatch_agent`'s own documented contract, so a future third adapter
    /// -- or a change to either existing one -- cannot silently drop an
    /// invariant for just one harness. Composed entirely from each adapter's
    /// own methods (`policy_args`, `default_sandbox_args`, `read_only_args`),
    /// with an additional single-use check for codex's sandbox and approval
    /// options. No real harness is launched.
    #[test]
    fn dispatch_agent_refuses_a_read_only_seat_on_an_empty_floor_adapter() {
        use crate::commands::workflow::agents::{
            AGENT_SCHEMA_VERSION, AgentManifest, AgentTask, ModelTier,
        };

        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let manifest = AgentManifest {
            schema_version: AGENT_SCHEMA_VERSION,
            id: "floor-probe".to_string(),
            version: 1,
            name: "Floor Probe".to_string(),
            description: "read-only seat without a floor".to_string(),
            role: "worker".to_string(),
            model_tier: ModelTier::Standard,
            read_only: true,
            required_capabilities: Vec::new(),
            optional_capabilities: Vec::new(),
            context_budget_bytes: 4096,
            instructions: "Do the thing.".to_string(),
            team_role: None,
            skills: Vec::new(),
        };
        let task = AgentTask {
            prompt: "do the thing".to_string(),
            repo: repo.path().to_path_buf(),
            model: None,
        };
        let adapter = select(Some("cursor-agent"), &[], &permissive_cfg()).expect("cursor-agent");
        let error = adapter
            .dispatch_agent(&manifest, &task)
            .expect_err("an empty read-only floor must refuse the seat");
        assert!(
            error.to_string().contains("cannot enforce read-only"),
            "{error}"
        );
    }

    #[test]
    fn dispatch_agent_invariants_hold_for_claude_and_codex() {
        use crate::commands::workflow::agents::{
            AGENT_SCHEMA_VERSION, AgentManifest, AgentTask, ModelTier,
        };
        use crate::commands::workflow::capability::CapabilityId;

        let repo = tempfile::tempdir().expect("tempdir");
        // Repo-layer narrowing (deny is always a permitted narrowing, never
        // `REPO_FORBIDDEN`): forces `policy_args` to be non-empty for both
        // adapters, so "the narrowing fold is applied" is not a vacuous
        // check against the shipped all-`Allow` default, where `policy_args`
        // returns empty for every adapter (see its own doc comment).
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[policy]\nrepo_fs_write = \"deny\"\n",
        )
        .expect("write ctx.toml");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let resolved_cfg =
            CtxConfig::load(repo.path(), &|k| std::env::var(k).ok()).expect("load resolved cfg");
        assert_eq!(
            resolved_cfg.policy.repo_fs_write,
            crate::commands::ctx::policy::Stance::Deny,
            "the repo-layer narrowing must actually be in effect"
        );

        let manifest = AgentManifest {
            schema_version: AGENT_SCHEMA_VERSION,
            id: "conformance-probe".to_string(),
            version: 1,
            name: "Conformance Probe".to_string(),
            description: "cross-adapter dispatch_agent invariants".to_string(),
            role: "worker".to_string(),
            model_tier: ModelTier::Standard,
            read_only: true,
            required_capabilities: vec![CapabilityId::RepoRead],
            optional_capabilities: Vec::new(),
            context_budget_bytes: 4096,
            instructions: "Do the thing.".to_string(),
            team_role: None,
            skills: Vec::new(),
        };
        let task = AgentTask {
            prompt: "do the thing".to_string(),
            repo: repo.path().to_path_buf(),
            model: None,
        };

        for name in ["claude", "codex"] {
            let adapter = select(Some(name), &[], &permissive_cfg())
                .unwrap_or_else(|e| panic!("{name}: {e}"));

            // Invariant 1: the capability check happens for every adapter,
            // not just one. Network access is `Stance::Deny` by default with
            // no config at all (see `EffectivePolicy`'s own doc comment), so
            // this needs no extra setup to be a real refusal.
            let mut denied = manifest.clone();
            denied.required_capabilities = vec![CapabilityId::NetworkAccess];
            let error = adapter
                .dispatch_agent(&denied, &task)
                .expect_err(&format!("{name}: a denied capability must refuse"));
            assert!(
                error.to_string().contains("network.access"),
                "{name}: {error}"
            );

            // Invariant 2: a satisfied seat dispatches, carrying the same
            // restrictions promised by `dispatch_agent`. A codex sandbox
            // is replaced in place; claude retains its baseline plus deny.
            let command = adapter
                .dispatch_agent(&manifest, &task)
                .unwrap_or_else(|e| panic!("{name}: a satisfied capability must dispatch: {e}"));
            let argv = flatten_command(command);

            let read_only_args = adapter.read_only_args();
            assert!(
                !read_only_args.is_empty(),
                "{name}: a read-only seat's floor must not be empty, or this invariant is vacuous"
            );
            assert!(
                find_subsequence(&argv, &read_only_args).is_some(),
                "{name}: the read-only floor must reach the command -- got {argv:?}"
            );

            let policy_args = adapter.policy_args(&resolved_cfg.policy, LaunchMode::Headless);
            assert!(
                !policy_args.is_empty(),
                "{name}: the forced repo_fs_write=deny narrowing must actually produce args"
            );
            let policy_pos = find_subsequence(&argv, &policy_args).unwrap_or_else(|| {
                panic!("{name}: the narrowing fold (policy_args) is missing from {argv:?}")
            });

            if name == "codex" {
                let sandbox: Vec<_> = argv
                    .windows(2)
                    .filter(|w| w[0] == "--sandbox")
                    .map(|w| w[1].as_str())
                    .collect();
                assert_eq!(sandbox, ["read-only"], "{argv:?}");
                assert_eq!(
                    argv.iter()
                        .filter(|arg| {
                            matches!(arg.as_str(), "--ask-for-approval" | "approval_policy=never")
                        })
                        .count(),
                    1,
                    "{argv:?}"
                );
            } else if resolved_cfg.sandbox.enabled {
                let sandbox_args = adapter.default_sandbox_args_for_role(
                    &resolved_cfg.sandbox,
                    &resolved_cfg.safety,
                    &[],
                    LaunchMode::Headless,
                    Some(crate::commands::ctx::prompt::PromptRole::Worker),
                );
                if !sandbox_args.is_empty() {
                    let sandbox_pos = find_subsequence(&argv, &sandbox_args).unwrap_or_else(|| {
                        panic!("{name}: sandbox args are missing from {argv:?}")
                    });
                    assert!(
                        sandbox_pos <= policy_pos,
                        "{name}: sandbox args must precede the narrowing fold -- got {argv:?}"
                    );
                }
            }
        }
    }

    /// Issue #92: a third adapter that overrides nothing must still be
    /// protected against the reparse-argv class, because the trait default
    /// now derives its answer from `resolve_program`'s own resolution of
    /// `program()` instead of assuming the permissive `false`. This adapter
    /// deliberately does not implement `launches_through_cmd_shim` at all.
    #[derive(Debug)]
    struct NoOverrideAdapter(String);

    impl AgentAdapter for NoOverrideAdapter {
        fn name(&self) -> &'static str {
            "no-override"
        }

        fn program(&self) -> &str {
            &self.0
        }

        fn provider(&self) -> &'static str {
            "no-override"
        }

        fn ready(&self) -> CtxResult<()> {
            Ok(())
        }

        fn detect(&self, _command: &[String]) -> bool {
            false
        }

        fn headless_cmd(&self, _prompt: &str, _session: &SessionId, _extra: &[String]) -> Command {
            Command::new("true")
        }

        fn interactive_cmd(&self, _initial_prompt: Option<&str>, _extra: &[String]) -> Command {
            Command::new("true")
        }

        fn distiller_cmd(&self, _model: &str) -> Command {
            Command::new("true")
        }

        fn read_only_args(&self) -> Vec<String> {
            Vec::new()
        }

        fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
            Vec::new()
        }

        fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
            PathBuf::new()
        }

        fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
            Vec::new()
        }

        fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
            StructuralContext::default()
        }

        fn compact_command(&self) -> Option<&'static str> {
            None
        }

        fn quit_sequence(&self) -> &'static str {
            ""
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
            TurnSignalSetup {
                env: Vec::new(),
                instructions: String::new(),
            }
        }
    }

    /// A direct, non-shim program never reports the shim shape, on any
    /// platform, even though this adapter never overrides the trait method --
    /// mirrors `ClaudeAdapter`/`CodexAdapter`'s own identically-named tests.
    #[test]
    fn an_adapter_with_no_override_reports_no_shim_for_a_direct_program() {
        let adapter = NoOverrideAdapter("/tmp/fake-agent".to_string());
        assert!(!adapter.launches_through_cmd_shim());
    }

    /// The core of issue #92: an adapter that implements nothing beyond the
    /// required trait methods -- no `launches_through_cmd_shim` override at
    /// all -- still reports the shim shape correctly for a real `.cmd` on
    /// Windows, because the trait default derives it from `resolve_program`.
    /// Before this fix the default was a hardcoded `false`, so this exact
    /// adapter shape would have shipped unprotected.
    #[cfg(windows)]
    #[test]
    fn an_adapter_with_no_override_is_still_protected_from_a_cmd_shim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("no-override-agent.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = NoOverrideAdapter(shim.display().to_string());
        assert!(
            adapter.launches_through_cmd_shim(),
            "a .cmd resolution must be reported as the shim shape even with no override"
        );
    }

    /// The trait default: an agent zirv has verified nothing about receives
    /// no base layer, rather than another agent's instructions.
    #[test]
    fn an_unverified_agent_receives_no_base_layer_by_default() {
        assert_eq!(
            NoOverrideAdapter(String::new()).base_system_prompt(OrchestratorWrites::Deny),
            None
        );
    }

    /// Issue #167: both real adapters now have their own base layer, and
    /// each is genuinely its own text -- neither ever hands the other
    /// agent's tool-specific instructions.
    #[test]
    fn each_real_adapter_receives_its_own_distinct_base_layer() {
        let claude_layer = claude::ClaudeAdapter::new(None)
            .base_system_prompt(OrchestratorWrites::Deny)
            .expect("claude has one of its own");
        let codex_layer = codex::CodexAdapter::new(None)
            .base_system_prompt(OrchestratorWrites::Deny)
            .expect("codex has one of its own, issue #167");
        assert_ne!(claude_layer, codex_layer);
        assert!(
            !codex_layer.contains("Agent tool") && !codex_layer.contains(".claude/agents"),
            "claude-only vocabulary must not reach codex's own layer"
        );
    }

    #[test]
    fn explicit_name_wins() {
        let adapter = select(Some("claude"), &[], &permissive_cfg()).expect("claude selects");
        assert_eq!(adapter.name(), "claude");
    }

    /// `agent_bin` is one global override applied to whichever adapter gets
    /// selected. Naming codex explicitly while `agent_bin` points at a real
    /// `claude` install (stale config left over from switching agents is the
    /// plausible way this happens) would otherwise launch claude's binary
    /// dressed up in codex's own `exec <prompt>` argv shape -- wrong account,
    /// wrong safety model, no error naming what happened. Both names appear
    /// in the refusal, and it is basename-only: the full path is never a
    /// factor.
    #[test]
    fn agent_bin_naming_a_different_adapter_than_selected_is_refused() {
        let mut cfg = permissive_cfg();
        cfg.agent_bin = Some("/opt/homebrew/bin/claude".to_string());
        let err = select(Some("codex"), &[], &cfg).expect_err("cross-adapter agent_bin refuses");
        let msg = err.to_string();
        assert!(
            msg.contains("claude"),
            "names the binary's own agent: {msg}"
        );
        assert!(
            msg.contains("codex"),
            "names the one that was selected: {msg}"
        );
    }

    /// The same collision reached through `resolve_default`'s own
    /// *configured* arm (`cfg.agent` set explicitly, just not on the CLI) --
    /// still a hard refusal, unlike the fallback loop below.
    #[test]
    fn agent_bin_naming_a_different_adapter_is_refused_through_the_default_fallback_too() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("codex".to_string());
        cfg.agent_bin = Some("claude.exe".to_string());
        let err = resolve_default(&cfg).expect_err("cross-adapter agent_bin refuses");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
    }

    /// Medium 2 (fix): with *no* `cfg.agent` configured, `resolve_default`'s
    /// own fallback loop tries `ADAPTERS` in registry order (`claude` first)
    /// -- before this fix, `agent_bin` naming a real codex install still hit
    /// claude first, and the cross-adapter guard's `?` aborted the whole
    /// fallback right there instead of continuing on to codex, the adapter
    /// that binary actually is. It must resolve to codex, not error.
    #[test]
    fn agent_bin_naming_codex_with_no_agent_configured_falls_through_to_codex() {
        let cfg = CtxConfig {
            agent_bin: Some("/definitely/not/a/real/path/codex".to_string()),
            ..permissive_cfg()
        };
        let (adapter, origin) =
            resolve_default(&cfg).expect("falls through past claude to codex, not an error");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// The other half of the same fix: a basename that names *no* registered
    /// adapter at all -- a stub path, or the `sh <fixture>.sh` wrapper shape
    /// this codebase's own tests use throughout -- is never a collision, no
    /// matter how unrelated it looks, and a value that happens to name the
    /// *same* adapter as the one selected (a differently located install) is
    /// explicitly fine too.
    #[test]
    fn agent_bin_naming_no_adapter_or_the_same_one_stays_allowed() {
        let cfg = permissive_cfg();
        assert_eq!(
            agent_bin_names_a_different_adapter(Some("/tmp/fake-codex"), "codex"),
            None,
            "a stub path matches nothing"
        );
        assert_eq!(
            agent_bin_names_a_different_adapter(
                Some("sh /repo/tests/fixtures/fake-codex-agent.sh"),
                "codex"
            ),
            None,
            "the wrapper shape's own basename is \"sh\", not an adapter name"
        );
        assert_eq!(
            agent_bin_names_a_different_adapter(Some("/opt/codex-beta/codex"), "codex"),
            None,
            "naming the selected adapter itself is not a collision"
        );

        let mut cfg = cfg;
        cfg.agent_bin = Some("/opt/codex-beta/codex".to_string());
        let adapter =
            select(Some("codex"), &[], &cfg).expect("same-adapter agent_bin is never refused");
        assert_eq!(adapter.name(), "codex");
    }

    #[test]
    fn detection_reads_the_wrapped_argv() {
        let cmd = vec![
            "/opt/homebrew/bin/claude".to_string(),
            "--resume".to_string(),
        ];
        let adapter = select(None, &cmd, &permissive_cfg()).expect("detect claude");
        assert_eq!(adapter.name(), "claude");
    }

    /// The property the fallback actually promises: whatever it picks is
    /// enabled and ready. Now that codex's own `ready()` succeeds too (see
    /// `CodexAdapter::ready`), both adapters qualify, so this also pins
    /// `ADAPTERS`' registry order (`("claude", ...)` first) as what actually
    /// decides the winner -- both are asserted, the property for its own
    /// sake and the concrete name because losing it silently would be a
    /// regression worth catching too.
    #[test]
    fn empty_command_defaults_to_claude() {
        let cfg = permissive_cfg();
        let adapter =
            select_with_presence(None, &[], &cfg, true, &everything_installed()).expect("default");
        assert!(
            cfg.agents.is_enabled(adapter.name()),
            "must be gate-enabled"
        );
        assert!(adapter.ready().is_ok(), "must be ready");
        assert_eq!(adapter.name(), "claude");
    }

    #[test]
    fn unknown_name_is_an_error_that_lists_the_options() {
        // "gemini" was this test's own unknown-name example before issue
        // #384 registered it for real; "not-a-real-agent" keeps testing the
        // same unknown-agent path without colliding with a now-real adapter.
        let err =
            select(Some("not-a-real-agent"), &[], &permissive_cfg()).expect_err("unknown agent");
        let msg = err.to_string();
        assert!(msg.contains("not-a-real-agent"), "got {msg}");
        assert!(
            msg.contains("claude"),
            "error should list known adapters: {msg}"
        );
    }

    /// Task A3: an agent named explicitly is refused, and the message names
    /// the layer that disabled it (mirrors the settings-layer wording tests
    /// in `settings.rs`; here the point is that `select` actually surfaces
    /// it, not the exact wording).
    #[test]
    fn a_disabled_agent_named_explicitly_is_refused_with_the_layer_that_disabled_it() {
        let cfg = cfg_disabling("codex");
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
        assert!(
            msg.contains(".settings.toml"),
            "names the file that disabled it: {msg}"
        );
    }

    /// The detection arm must refuse, not silently fall back to claude, the
    /// same invariant `detecting_codex_argv_does_not_silently_fall_back_to_claude`
    /// pins for the unready case.
    #[test]
    fn a_disabled_agent_detected_on_the_argv_does_not_fall_back_to_the_default() {
        let cfg = cfg_disabling("codex");
        let cmd = vec!["codex".to_string(), "exec".to_string(), "go".to_string()];
        let err = select(None, &cmd, &cfg).expect_err("must not misroute to claude");
        assert!(err.to_string().contains("codex"), "got {err}");
    }

    /// G: `select`'s empty-command default no longer silently lands on a
    /// different provider just because the repo checkout narrowed claude
    /// off the table -- codex's own `ready()` succeeding too used to make
    /// the fallback pick it automatically, which handed a repo checkout the
    /// power to select which vendor account gets spent. It must refuse
    /// instead, naming both adapters and the fix, exercised here through the
    /// public `select` entry point rather than `resolve_default` directly.
    #[test]
    fn the_default_fallback_refuses_rather_than_silently_switching_provider() {
        let cfg = cfg_disabling("claude");
        let err = select_with_presence(None, &[], &cfg, true, &everything_installed())
            .expect_err("a repo may narrow, not select");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("--agent"), "must say how to fix it: {msg}");
    }

    /// The gate is checked before `ready()`: a disabled-and-unready agent
    /// (codex, always) must report the disable, not "not implemented yet".
    #[test]
    fn the_disable_is_reported_before_an_adapters_own_readiness() {
        let cfg = cfg_disabling("codex");
        let err = select(Some("codex"), &[], &cfg).expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(
            !msg.contains("not implemented yet"),
            "the gate must win over ready(): {msg}"
        );
        assert!(msg.contains("disabled"), "got {msg}");
    }

    #[test]
    fn registry_exposes_every_registered_adapter() {
        let names: Vec<&str> = ADAPTERS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "claude",
                "codex",
                "copilot",
                "cursor-agent",
                "droid",
                "gemini",
                "goose",
                "grok",
                "kimi",
                "muse",
                "opencode",
                "pi",
                "qwen"
            ]
        );
    }

    /// The registry table is the one place a new adapter is wired in: `all`
    /// must produce exactly one instance per table entry, in table order,
    /// with matching names -- otherwise `all` and `ADAPTERS` could drift.
    #[test]
    fn adding_an_adapter_is_one_entry_in_the_constructor_table() {
        let instances = all(None);
        assert_eq!(instances.len(), ADAPTERS.len());
        for (instance, (name, _)) in instances.iter().zip(ADAPTERS.iter()) {
            assert_eq!(instance.name(), *name);
        }
    }

    /// A provider slug names a usage file, so it has to already *be* a slug:
    /// lowercase `[a-z0-9-]`, non-empty, and unchanged by the sanitiser that
    /// turns it into a file name. It is also the account, not the program --
    /// claude's is `anthropic`, not `claude`.
    #[test]
    fn every_adapter_names_the_account_its_limits_belong_to() {
        for adapter in all(None) {
            let provider = adapter.provider();
            assert!(!provider.is_empty(), "{} has no provider", adapter.name());
            assert_eq!(
                crate::commands::ctx::state::provider_slug(provider),
                provider,
                "{provider} is not already a filesystem-safe lowercase slug"
            );
        }

        let claude = claude::ClaudeAdapter::new(None);
        assert_ne!(
            claude.provider(),
            claude.name(),
            "the provider is the account, not the binary: two harnesses can share one"
        );
    }

    #[test]
    fn the_registry_names_are_unique_and_non_empty() {
        let names: Vec<&str> = ADAPTERS.iter().map(|(name, _)| *name).collect();
        for name in &names {
            assert!(!name.is_empty(), "no adapter may have an empty name");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate adapter name in {names:?}"
        );
    }

    #[test]
    fn an_empty_command_falls_back_to_the_first_enabled_and_ready_adapter() {
        let (adapter, origin) =
            resolve_default_with_presence(&permissive_cfg(), &everything_installed())
                .expect("a default exists");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G: now that codex's own `ready()` only checks that its program
    /// resolves (exactly like claude's, see `CodexAdapter::ready`),
    /// disabling claude via a repo-only `.settings.toml` does not leave the
    /// fallback with nothing enabled-and-ready -- codex, next in registry
    /// order, would qualify. `resolve_default` must refuse rather than
    /// silently landing on it: the repo checkout narrowed claude off the
    /// table, but selecting codex *instead* is not the repo's call to make
    /// (`AgentGate::disabled_only_by_repo`).
    #[test]
    fn the_fallback_refuses_to_silently_switch_provider_when_the_repo_disabled_the_default() {
        let cfg = cfg_disabling("claude");
        let err = resolve_default_with_presence(&cfg, &everything_installed())
            .expect_err("a repo may narrow, not select");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "names the narrowed adapter: {msg}");
        assert!(
            msg.contains("codex"),
            "names what it would have silently picked: {msg}"
        );
        assert!(msg.contains("--agent"), "says how to fix it: {msg}");
    }

    /// G2 (fix): the "would otherwise have been the default agent" refusal
    /// must not fire when the repo-disabled adapter was never actually a
    /// candidate -- here claude is *both* repo-disabled *and* genuinely
    /// unready (the same PATH/PATHEXT rig `readiness_note_and_the_fallback_
    /// skip_both_stay_covered_when_an_adapter_is_genuinely_unready` uses), so
    /// disabling it changed nothing: codex was always going to be the
    /// fallback either way, and the refusal's own premise would be false.
    #[cfg(windows)]
    #[test]
    fn the_narrowed_refusal_does_not_fire_for_an_adapter_that_was_never_ready_anyway() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let cfg = cfg_disabling("claude");
        let (adapter, origin) = resolve_default_with_presence(&cfg, &everything_installed())
            .expect("codex qualifies; claude was never a real candidate");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Final wave item 3: the same false-premise class as the test above,
    /// but reached through `agent_bin` instead of an unresolvable `PATH`.
    /// Claude is repo-disabled, and `agent_bin`'s own basename names codex,
    /// not claude -- so the pre-check used to build `ClaudeAdapter::new(bin)`
    /// (a claude adapter whose `program` actually points at a codex binary)
    /// and ask *that* whether it is `ready()`, which can genuinely answer
    /// yes without claude's own real binary ever being consulted at all.
    /// Recording `repo_narrowed` from that would refuse with a false claim
    /// ("claude would otherwise have been the default agent") over a
    /// candidate `agent_bin_names_a_different_adapter` was always going to
    /// refuse anyway (Medium 2). It must instead land on codex.
    #[test]
    fn the_narrowed_refusal_does_not_fire_when_agent_bin_names_a_different_adapter() {
        let cfg = CtxConfig {
            agent_bin: Some("/definitely/not/a/real/path/codex".to_string()),
            ..cfg_disabling("claude")
        };
        let (adapter, origin) = resolve_default(&cfg)
            .expect("codex qualifies; agent_bin never actually named claude's own binary");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G: the refusal is specific to a *repo-only* disable. An operator who
    /// disabled claude themselves (home file or environment) has already
    /// made the choice the fallback would otherwise be accused of making for
    /// them, so codex is picked normally, exactly as before this fix.
    #[test]
    fn an_operator_disable_still_falls_through_normally() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let (adapter, origin) = resolve_default_with_presence(&cfg, &everything_installed())
            .expect("the operator's own choice");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Disabling every known adapter leaves nothing to fall back to; the
    /// error must aggregate one line per adapter naming its own reason,
    /// reusing the gate's refusal text and each adapter's own `ready()` text
    /// rather than inventing new wording. Issue #386: `pi` disabled too --
    /// otherwise, with pi left enabled and `ready()` (it fails open on a
    /// missing binary, same as every adapter), the loop in `resolve_default`
    /// would reach pi as an enabled-and-ready candidate and return the
    /// EARLIER "repo may narrow but not silently choose 'pi' for you"
    /// refusal instead of ever reaching the "no agent is both enabled and
    /// ready" branch this test means to exercise.
    #[test]
    fn when_no_adapter_is_both_enabled_and_ready_the_error_names_each_one_and_why() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        // Every registered adapter must be disabled, not just claude/codex --
        // issue #384 registered a THIRD adapter (gemini) whose own `ready()`
        // fails open on a missing binary (`resolve_program`'s own contract),
        // so leaving it enabled would let `resolve_default` silently pick it
        // instead of refusing, defeating this test's own "nothing qualifies"
        // premise.
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            ADAPTERS
                .iter()
                .map(|(name, _)| format!("[agents.{name}]\nenabled = false\n"))
                .collect::<String>(),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let err = resolve_default(&cfg).expect_err("all disabled");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(msg.contains("opencode"), "must name opencode: {msg}");
        assert!(
            msg.contains("disabled"),
            "must say why claude lost out: {msg}"
        );
        assert!(
            msg.contains("not implemented yet") || msg.contains("disabled"),
            "must say why codex lost out: {msg}"
        );
    }

    /// The other half of the same error: absence is only *part* of the
    /// story here (claude is disabled by the repo, the rest are missing), so
    /// the plain "no harness is installed" sentence would assert something
    /// this function cannot know -- a disabled adapter may well be sitting
    /// on `PATH`. It names what is missing and claims nothing about the
    /// rest, the same discipline `Liveness`'s own doc comment holds every
    /// other surface to.
    #[test]
    fn a_partly_disabled_partly_missing_registry_never_claims_nothing_is_installed() {
        let cfg = cfg_disabling("claude");
        let err = resolve_default_with_presence(&cfg, &only_installed(&[]))
            .expect_err("nothing is both enabled and installed");
        let msg = err.to_string();
        assert!(
            msg.contains("not installed on this machine: codex"),
            "names what is actually missing: {msg}"
        );
        assert!(
            !msg.contains("no harness is installed"),
            "claude is disabled, not known to be absent: {msg}"
        );
        assert!(msg.contains("--agent"), "still says how to name one: {msg}");
    }

    /// Issue #690's other half: presence gates the choice of what to
    /// *launch*, never the naming of an adapter. `select_for_identity` is
    /// what the Stop hook's screening, `zirv ctx score` and the usage
    /// readout ask, and on a machine where the probe finds nothing they must
    /// answer exactly as they did before presence existed -- a transcript
    /// written by claude is claude's whether or not `claude` is on this
    /// process's `PATH`, and a hook subprocess routinely inherits a reduced
    /// one.
    #[test]
    fn naming_an_adapter_never_asks_whether_it_is_installed() {
        let adapter = select_for_identity(None, &[], &permissive_cfg())
            .expect("naming an adapter is not a launch");
        assert_eq!(adapter.name(), "claude");
    }

    /// The passthrough half of the same rule, and the one CLAUDE.md makes
    /// non-negotiable ("supervision failure is pure passthrough"): a command
    /// the operator supplied that no adapter claims is zirv labelling
    /// someone else's program, not choosing a harness, so an empty machine
    /// must never turn `wrap --no-supervise -- echo hi` into a refusal. With
    /// nothing to pass through it is the launch case again, and still
    /// refuses.
    ///
    /// Both calls answer `adapter_builds_launch` through
    /// [`adapter_builds_launch`] itself rather than writing `false`/`true`
    /// out, because it is `select`'s -- and so `wrap`'s -- derivation that
    /// is on trial here, not `select_with_presence`'s handling of an answer
    /// already given. Widening that derivation to `exec`'s (a flags-only
    /// argv is adapter-built) would make this test fail, which is exactly
    /// what it is for.
    #[test]
    fn an_operators_own_command_is_never_refused_for_a_missing_harness() {
        let command = vec!["echo".to_string(), "hello".to_string()];
        let adapter = select_with_presence(
            None,
            &command,
            &permissive_cfg(),
            adapter_builds_launch(&command),
            &only_installed(&[]),
        )
        .expect("passthrough must never be refused");
        assert_eq!(adapter.name(), "claude");

        select_with_presence(
            None,
            &[],
            &permissive_cfg(),
            adapter_builds_launch(&[]),
            &only_installed(&[]),
        )
        .expect_err("choosing a harness to launch still needs one to exist");
    }

    /// The case `command.is_empty()` alone got wrong: `zirv ctx exec --
    /// --model x` hands over an argv that names no program, only flags that
    /// `exec` appends to `adapter.program()`. That is zirv choosing a
    /// harness to launch every bit as much as an empty argv is, so on a
    /// machine with codex and no claude it must land on the one that is
    /// actually there. Derived from `command` here, this said "operator's
    /// own program, do not consult presence", kept the absent default, and
    /// left `exec`'s launch pre-flight to refuse a harness the operator
    /// never asked for over one they had installed.
    ///
    /// The stated machine is `only_installed(&["codex"])`, so nothing here
    /// depends on what this developer has: a caller that stopped consulting
    /// presence would answer "claude" and fail on the assertion below.
    #[test]
    fn a_flags_only_command_is_a_harness_this_machine_has_to_have() {
        let command = vec!["--model".to_string(), "x".to_string()];
        let adapter = select_with_presence(
            None,
            &command,
            &permissive_cfg(),
            true,
            &only_installed(&["codex"]),
        )
        .expect("a machine with codex installed can launch codex");
        assert_eq!(adapter.name(), "codex");
    }

    /// The other side of that widening, and the reason it is safe: stating
    /// `adapter_builds_launch` does not reorder anything. On an ordinary
    /// machine that does have the first candidate, the same flags-only argv
    /// still resolves to registry order's own answer -- presence gets a say
    /// only about candidates it can rule out, never a preference between
    /// two installed ones.
    #[test]
    fn a_flags_only_command_still_takes_the_first_candidate_that_is_installed() {
        let command = vec!["--model".to_string(), "x".to_string()];
        let adapter = select_with_presence(
            None,
            &command,
            &permissive_cfg(),
            true,
            &only_installed(&["claude", "codex"]),
        )
        .expect("claude is installed on this stated machine");
        assert_eq!(adapter.name(), "claude");

        let adapter = select_with_presence(
            None,
            &command,
            &permissive_cfg(),
            true,
            &everything_installed(),
        )
        .expect("everything is installed on this stated machine");
        assert_eq!(adapter.name(), "claude");
    }

    /// The fallback is only reached when neither an explicit `--agent` nor
    /// detection named an adapter; either one must bypass it entirely.
    #[test]
    fn an_explicit_or_detected_agent_still_bypasses_the_fallback_entirely() {
        let cfg = cfg_disabling("claude");

        // H3: `resolve_default`'s own fallback would *refuse* under this
        // exact cfg (G: claude is disabled only by the repo layer, and
        // codex would otherwise be silently picked instead) -- proving that
        // if `select(Some("codex"), ...)` below were ever accidentally
        // routed through the fallback instead of truly bypassing it, this
        // test would see that refusal, not a quiet "codex" answer. The two
        // assertions below are provably distinguishable outcomes, not the
        // same value reached two different ways.
        resolve_default_with_presence(&cfg, &everything_installed())
            .expect_err("the fallback itself must refuse here");

        // Explicit name: codex is still enabled by this gate and now
        // resolves successfully, so it is selected directly without ever
        // consulting the fallback.
        let adapter = select(Some("codex"), &[], &cfg).expect("codex is enabled and ready");
        assert_eq!(adapter.name(), "codex");

        // Detection: an argv that names claude explicitly is refused for
        // being disabled, not silently redirected into the fallback.
        let cmd = vec!["/usr/bin/claude".to_string()];
        let err = select(None, &cmd, &cfg).expect_err("claude is disabled");
        assert!(err.to_string().contains("disabled"), "got {err}");
    }

    #[test]
    fn resolve_default_reports_which_rule_chose_the_adapter() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("claude".to_string());
        let (adapter, origin) = resolve_default_with_presence(&cfg, &everything_installed())
            .expect("claude is configured");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::Configured);

        let (adapter, origin) =
            resolve_default_with_presence(&permissive_cfg(), &everything_installed())
                .expect("fallback picks one");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G3 (issue #690): an operator whose only harness is codex, with no
    /// `agent` configured, used to get claude's own "program 'claude' not
    /// found" and no way forward -- registry order alone decided the answer,
    /// and nothing asked whether that answer exists on this machine. The
    /// fallback now drops a confidently absent candidate, and says so in the
    /// origin: choosing a different vendor for an operator whose machine
    /// leaves only one choice is legitimate; doing it without a word is not.
    #[test]
    fn the_fallback_skips_a_harness_that_is_not_installed_and_names_it() {
        let (adapter, origin) =
            resolve_default_with_presence(&permissive_cfg(), &only_installed(&["codex"]))
                .expect("codex is installed, so there is an answer to give");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(
            origin,
            DefaultOrigin::FirstInstalledReady {
                not_found: "claude"
            },
            "the origin has to carry the missing harness, or no surface can announce it"
        );
    }

    /// Presence only ever *removes* candidates. With both installed,
    /// registry order still decides exactly as it always did, and the origin
    /// is the unchanged one -- the common case gains no new wording on any
    /// surface.
    #[test]
    fn with_both_harnesses_installed_registry_order_still_decides() {
        let (adapter, origin) =
            resolve_default_with_presence(&permissive_cfg(), &only_installed(&["claude", "codex"]))
                .expect("a default exists");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// Fail-open, the discipline `Liveness` and `program_is_present` already
    /// hold every other probe to: a verdict the probe could not reach must
    /// never cost an operator a harness they may perfectly well have (codex
    /// on this repo's own Windows dev machine lives outside `PATH`). An
    /// `Unknown` for everything has to leave the answer identical to the
    /// pre-#690 one.
    #[test]
    fn an_undecidable_probe_changes_nothing_at_all() {
        let (adapter, origin) =
            resolve_default_with_presence(&permissive_cfg(), &nothing_decidable())
                .expect("an inconclusive probe never removes a candidate");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G3 and `agent_bin`: an operator who pointed zirv at a program has
    /// already made the choice presence exists to inform, and the override
    /// need not be a path a `stat` can answer at all -- the `sh
    /// <wrapper>.sh` shape this codebase's own fixtures use throughout
    /// resolves to nothing on disk. A probe that would call every candidate
    /// absent must therefore not be allowed to empty the fallback: with an
    /// override in effect, presence is not consulted at all.
    #[test]
    fn an_agent_bin_override_is_never_second_guessed_by_presence() {
        let cfg = CtxConfig {
            agent_bin: Some("sh /repo/tests/fixtures/fake-claude-agent.sh".to_string()),
            ..permissive_cfg()
        };
        let (adapter, origin) = resolve_default_with_presence(&cfg, &only_installed(&[]))
            .expect("the override names the program; presence has nothing to say about it");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }

    /// G3 meets G2: a repo-disabled adapter that is not installed either was
    /// never a candidate this fallback could have landed on, so "a repo may
    /// narrow but not choose for you" has no premise left to refuse over --
    /// the next adapter is chosen, and announced. The sibling case (repo-
    /// disabled and genuinely installed) still refuses; that is `the_
    /// fallback_refuses_to_silently_switch_provider_when_the_repo_disabled_
    /// the_default`, which now states the machine it assumes rather than
    /// inheriting one.
    #[test]
    fn a_repo_disabled_harness_that_is_not_installed_refuses_nothing() {
        let cfg = cfg_disabling("claude");
        let (adapter, origin) = resolve_default_with_presence(&cfg, &only_installed(&["codex"]))
            .expect("claude was never a candidate here; codex is");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(
            origin,
            DefaultOrigin::FirstInstalledReady {
                not_found: "claude"
            },
            "the operator still has to be told which harness is missing"
        );
    }

    /// Presence is consulted in the fallback arm and nowhere else. A harness
    /// the operator named -- `agent =` in their own config, or `--agent` --
    /// comes back exactly as it did before, missing binary and all, so what
    /// they get is their own harness's launch error rather than a session
    /// quietly opened against another vendor's account. (`ready()` fails
    /// open on a missing binary by design, so that error lands at the spawn,
    /// via `format_launch_error`, not here.)
    #[test]
    fn an_explicitly_chosen_harness_is_never_swapped_for_an_installed_one() {
        let mut cfg = permissive_cfg();
        cfg.agent = Some("claude".to_string());
        let (adapter, origin) = resolve_default_with_presence(&cfg, &only_installed(&["codex"]))
            .expect("the configured arm does not consult presence");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(origin, DefaultOrigin::Configured);

        let adapter = select_with_presence(
            Some("claude"),
            &[],
            &permissive_cfg(),
            true,
            &only_installed(&["codex"]),
        )
        .expect("an explicit --agent does not consult presence either");
        assert_eq!(adapter.name(), "claude");
    }

    /// With nothing installed at all there is no honest answer left to give,
    /// so the aggregate error has to say the plain thing -- no adapter's own
    /// `ready()` text ever will, since `ready()` fails open on a missing
    /// binary -- and name the way out.
    #[test]
    fn nothing_installed_at_all_is_an_error_that_says_so_and_how_to_fix_it() {
        let err = resolve_default_with_presence(&permissive_cfg(), &only_installed(&[]))
            .expect_err("no harness exists on this machine");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "names each candidate: {msg}");
        assert!(msg.contains("codex"), "names each candidate: {msg}");
        assert!(
            msg.contains("no harness is installed on this machine"),
            "says the plain thing rather than only each adapter's own text: {msg}"
        );
        assert!(msg.contains("PATH"), "says how to install one: {msg}");
        assert!(msg.contains("--agent"), "says how to name one: {msg}");
    }

    /// The gate wrap and exec use before injecting: a command that matches no
    /// adapter, with no explicit `--agent` to back it, must not be treated as
    /// a match just because `select` had to default to one.
    #[test]
    fn an_undetected_command_with_no_explicit_agent_does_not_match() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(!command_matches_adapter(&adapter, false, &command));
    }

    #[test]
    fn an_explicit_agent_matches_regardless_of_the_command() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["echo".to_string(), "hello".to_string()];
        assert!(command_matches_adapter(&adapter, true, &command));
    }

    #[test]
    fn a_detected_command_matches_even_without_an_explicit_agent() {
        let adapter = claude::ClaudeAdapter::new(None);
        let command = vec!["/opt/homebrew/bin/claude".to_string()];
        assert!(command_matches_adapter(&adapter, false, &command));
    }

    // Worker model resolution (`resolve_worker_model`/`worker_model_args`):
    // the delegated-headless-worker analogue of `resolve_review_model`
    // above, but with a fixed adapter-owned default instead of a ladder.

    /// The pure latch primitive `announce_sandbox_residual_once` builds on:
    /// exactly one caller ever wins, regardless of how many times it is
    /// asked, so the announcement itself cannot fire more than once per
    /// process even though `handoff::run_model` (and `read_only_args_for_
    /// agent_name`) call it on every single distiller/reviewer spawn.
    #[test]
    fn claim_once_wins_exactly_once() {
        let latch = std::sync::atomic::AtomicBool::new(false);
        assert!(claim_once(&latch), "the first call claims the latch");
        assert!(
            !claim_once(&latch),
            "a second call must find it already claimed"
        );
        assert!(!claim_once(&latch), "and every call after that too");
    }

    /// Claude's own distiller/reviewer argv must be byte-for-byte unchanged
    /// by issue #89: `sandbox_residual_note` stays `None` (the trait
    /// default), so `announce_sandbox_residual_once` is always a no-op for
    /// it regardless of the latch, and nothing about `read_only_args`/
    /// `distiller_cmd` changed for this adapter at all.
    #[test]
    fn claude_has_no_sandbox_residual_to_announce() {
        let adapter = claude::ClaudeAdapter::new(None);
        assert_eq!(adapter.sandbox_residual_note(), None);
        assert_eq!(
            adapter.read_only_args(),
            vec!["--disallowedTools=Write,Edit,Bash,NotebookEdit".to_string()],
            "unchanged by issue #89"
        );
    }

    /// `announce_sandbox_residual_once` must be a safe no-op for an adapter
    /// with nothing to disclose -- it must not touch the shared latch at
    /// all, so a claude call never steals the one announcement a later
    /// codex call in the same process is entitled to.
    #[test]
    fn announce_sandbox_residual_once_never_claims_the_latch_for_an_adapter_with_no_residual() {
        let latch = std::sync::atomic::AtomicBool::new(false);
        // Exercise the same "no residual -> no claim" branch
        // `announce_sandbox_residual_once` itself takes, against a
        // caller-owned latch so this is independent of whatever the real
        // process-wide static has already done in this test binary.
        let adapter = claude::ClaudeAdapter::new(None);
        assert!(adapter.sandbox_residual_note().is_none());
        assert!(
            !latch.load(std::sync::atomic::Ordering::SeqCst),
            "an adapter with nothing to report must never reach the claim step"
        );
    }

    // -- scratchpad_rules (issue #104) ---------------------------------
}
