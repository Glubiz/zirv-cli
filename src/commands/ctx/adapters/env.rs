//! Supervisor env vars, launch-mode posture, and model-flag argv parsing shared
//! across adapters.

/// How an adapter arranges for turn-boundary events to reach a supervisor's
/// socket. `env` is injected into the launched agent so the hook that runs
/// inside it can find the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSignalSetup {
    pub env: Vec<(String, String)>,
    pub instructions: String,
}

pub const SOCKET_ENV: &str = "ZIRV_CTX_SOCKET";

pub const SESSION_ENV: &str = "ZIRV_CTX_SESSION";

/// Tells a spawned session which agent it is running as. Deliberately the
/// same name as `ctx.toml`'s own `agent` config key (`ZIRV_CTX_AGENT` in
/// `config::ENV_MAP`): it states the same fact from the other direction, so a
/// nested `zirv ctx ...` invocation inside a worker's own child processes
/// defaults to that worker's own harness rather than re-resolving from
/// scratch. Read by `mail::run_send`/`mail::run_inbox` to identify the
/// calling session without requiring an explicit `--to`/`--agent` flag.
pub const AGENT_ENV: &str = "ZIRV_CTX_AGENT";

/// Pin the orchestrator seat model so the hook can refuse subagent dispatch that would inherit it; scrub this pin from workers.
pub const SEAT_MODEL_ENV: &str = "ZIRV_CTX_SEAT_MODEL";

/// Pin the launch role for hook authority checks; only `orchestrator` restricts writes, and nested launches must scrub it. (#328, #334, #537)
/// Every guard compares only against the literal `"orchestrator"`; single/worker/
/// sub-orchestrator are equivalent for authority purposes.
pub const SEAT_ROLE_ENV: &str = "ZIRV_CTX_SEAT_ROLE";

/// Marks a proxy-decided launch so intake does not run twice; scrub on nested launches. (#753)
pub const PROXY_DECIDED_ENV: &str = "ZIRV_CTX_PROXY_DECIDED";

/// Pin interactivity from zirv's launch record, not the harness's reported permission mode; nested launches must decide afresh. (#147)
pub const LAUNCH_MODE_ENV: &str = "ZIRV_CTX_LAUNCH_MODE";

/// The one value [`LAUNCH_MODE_ENV`] is ever set to. Any other value, or its
/// absence, reads as "not provably zirv-interactive-launched" -- absence is
/// the fail-closed default, not a second, spoofable "false" value.
pub const LAUNCH_MODE_INTERACTIVE_VALUE: &str = "interactive";

/// Marks unattended launches so interactive skills are refused; only the exact value `"1"` counts, and nested launches scrub it.
/// Means "nobody is present to answer a prompt", not "this run has no visible terminal" --
/// spawn topology and prompt-answerability are different questions.
pub const HEADLESS_ENV: &str = "ZIRV_CTX_HEADLESS";

/// Marks a model call or unattended loop zirv itself spawned (distiller, loop cycle), never a
/// user's session; the prompt hook starts no workflow in it. Only `"1"` counts; nested launches scrub it.
pub const INTERNAL_ENV: &str = "ZIRV_CTX_INTERNAL";

/// The `(key, value)` pair a real interactive-launch seam pushes into its
/// child's env vector -- `None` for [`LaunchMode::Headless`], so a headless
/// launch adds nothing rather than a second, spoofable "not interactive"
/// value alongside the pin.
pub fn launch_mode_pin_env(mode: LaunchMode) -> Option<(String, String)> {
    match mode {
        LaunchMode::Interactive => Some((
            LAUNCH_MODE_ENV.to_string(),
            LAUNCH_MODE_INTERACTIVE_VALUE.to_string(),
        )),
        LaunchMode::Headless => None,
    }
}

/// The mirror image of [`launch_mode_pin_env`]: the `(key, value)` pair a
/// launch seam pushes to mark its child as unattended -- `Some` for
/// [`LaunchMode::Headless`], `None` for a launch a human is watching. Every
/// seam that resolves a `LaunchMode` gets the marker from this one function
/// rather than deciding for itself whether it counts as headless.
pub fn headless_marker_env(mode: LaunchMode) -> Option<(String, String)> {
    match mode {
        LaunchMode::Interactive => None,
        LaunchMode::Headless => Some((HEADLESS_ENV.to_string(), "1".to_string())),
    }
}

/// Recognize separated, `=`-joined, and attached short model flags consistently for launch and policy checks.
pub(crate) enum ModelFlagForm<'a> {
    Separated,
    Joined(&'a str),
}

/// Classifies `arg`, or `None` when it is not a model flag at all.
///
/// The attached short form (`-mopus`) is recognised only when `arg` is not
/// itself a `--`-prefixed long flag -- `--model-foo` starts with `-m` too,
/// once its own leading `-` is peeled, and must not match -- and carries at
/// least one character of value (`arg.len() > 2`, so a bare `-m` is
/// `Separated`, not an attached value of `""`).
pub(crate) fn classify_model_flag(arg: &str) -> Option<ModelFlagForm<'_>> {
    if arg == "--model" || arg == "-m" {
        return Some(ModelFlagForm::Separated);
    }
    if let Some(value) = arg.strip_prefix("--model=") {
        return Some(ModelFlagForm::Joined(value));
    }
    if let Some(value) = arg.strip_prefix("-m=") {
        return Some(ModelFlagForm::Joined(value));
    }
    if !arg.starts_with("--") && arg.starts_with("-m") && arg.len() > 2 {
        return Some(ModelFlagForm::Joined(&arg[2..]));
    }
    None
}

/// Whether a human can answer an approval prompt; headless is the safe default when that cannot be proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LaunchMode {
    /// `zirv chat`, `zirv ctx wrap`, a dashboard pane, a live handover swap:
    /// the harness's own TUI is on a terminal the operator is watching, so an
    /// `Ask` verdict becomes a real prompt they can answer.
    Interactive,
    /// `zirv ctx exec`, `zirv ctx loop`, `zirv ctx agent`: nobody is present,
    /// so an `Ask` verdict is an unanswerable prompt and must fail closed.
    Headless,
}

// Both accessors serve policy reports and adapter launch policy. (#230)
impl LaunchMode {
    pub fn label(self) -> &'static str {
        match self {
            LaunchMode::Interactive => "interactive",
            LaunchMode::Headless => "headless",
        }
    }

    pub fn is_interactive(self) -> bool {
        matches!(self, LaunchMode::Interactive)
    }
}

/// The last model-flag occurrence in `flags`, in any form `classify_model_
/// flag` recognises -- CLI last-wins semantics, the same rule a real argv
/// parser applies when a flag is repeated, honored across mixed spellings
/// (a later `-mhaiku` still overrides an earlier `--model opus`). `None`
/// when `flags` names no model at all, or when a trailing bare `--model`/
/// `-m` has nothing after it to be its value -- a dangling flag with no
/// value contributes nothing, it does not clear an earlier match.
///
/// Recognises codex's `-m` short alias (all three forms) as well as
/// claude's long `--model`, unlike the version of this function before FIX
/// A: this feeds `seat_model_env`, and a codex-adapter launch built with a
/// bare `-m <expensive>`/`-m=<expensive>`/`-m<expensive>` passthrough used
/// to export no seat env at all, leaving the pretool guard blind to it.
///
/// `pub(crate)`: `agent::run_with` (issue #155, Phase 2) is a second caller,
/// reading the model actually launched with back out of the effective argv
/// for the delegation checkpoint record -- same scan, no reason for a
/// second copy of it.
pub(crate) fn last_model_flag(flags: &[String]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < flags.len() {
        match classify_model_flag(&flags[i]) {
            Some(ModelFlagForm::Separated) => {
                if let Some(value) = flags.get(i + 1) {
                    found = Some(value.as_str());
                }
                i += 2;
                continue;
            }
            Some(ModelFlagForm::Joined(value)) => {
                found = Some(value);
            }
            None => {}
        }
        i += 1;
    }
    found
}

/// Return a model only when every flag token belongs to valid model selection; reject dangling or flag-shaped values.
pub(crate) fn model_only_flags(flags: &[String]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i < flags.len() {
        match classify_model_flag(&flags[i]) {
            Some(ModelFlagForm::Separated) => {
                found = Some(flags.get(i + 1)?.as_str());
                i += 2;
            }
            Some(ModelFlagForm::Joined(value)) => {
                found = Some(value);
                i += 1;
            }
            None => return None,
        }
    }
    found
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.starts_with('-'))
}

/// The `SEAT_MODEL_ENV` pair a launch exports, or nothing. Pure, so which
/// launches disclose a seat is testable without a pty.
///
/// Only an `Orchestrator` or (issue #537 T3) `Single` launch with a
/// non-blank resolved model discloses one: a `Worker`/`SubOrchestrator` is
/// not a seat that dispatches subagents, and with no resolved model the
/// harness picks its own default, which zirv cannot name and therefore must
/// not claim to. A `Single` seat is included alongside `Orchestrator`
/// because it is just as interactive and just as able to fork a native
/// subagent through its own harness's mechanism -- `hook::pretool_decision`
/// (the expensive-seat subagent guard) and `zirv ctx status` both need to
/// see its model exactly as reliably as an Orchestrator's.
///
/// The resolved model prefers an operator-passed `--model`/`--model=` in
/// `flags` (the last occurrence, CLI last-wins) over `cfg_model`
/// (`cfg.chat.model`): `flags` is the argv the launch actually uses, built by
/// `extra_with_model` from `cfg_model` and then the operator's own trailing
/// flags appended after it, so an operator passthrough like `zirv chat --
/// --model fable` with no `chat.model` configured must still disclose the
/// seat it actually launches on, and a configured `chat.model` that an
/// operator's own passthrough then overrides must disclose the flag's value,
/// not the configured one -- both directions the guard was blind to when
/// this only ever read `cfg.chat.model`.
pub fn seat_model_env(
    role: super::super::prompt::PromptRole,
    flags: &[String],
    cfg_model: Option<&str>,
) -> Vec<(String, String)> {
    use super::super::prompt::PromptRole;
    if !matches!(role, PromptRole::Orchestrator | PromptRole::Single) {
        return Vec::new();
    }
    let resolved = last_model_flag(flags).or(cfg_model);
    match resolved.map(str::trim).filter(|m| !m.is_empty()) {
        Some(model) => vec![(SEAT_MODEL_ENV.to_string(), model.to_string())],
        None => Vec::new(),
    }
}

/// The `SEAT_ROLE_ENV` pair a launch exports -- unlike `seat_model_env`,
/// unconditional for every role, since a hook process (`zirv ctx hook
/// pretool`, `zirv ctx safety check`) and `zirv ctx agent` need to tell a
/// worker or sub-orchestrator seat apart from an orchestrator one just as
/// reliably as they need to detect an orchestrator seat at all. Only the
/// value `"orchestrator"` ever gates any behaviour (issues #328/#334). Pure,
/// so which role a launch discloses is testable without a pty.
pub fn seat_role_env(role: super::super::prompt::PromptRole) -> Vec<(String, String)> {
    vec![(SEAT_ROLE_ENV.to_string(), role.label().to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interactive/headless seam itself (2026-08-24, cross-harness
    /// permissions): the enum every real-launch call site now has to answer
    /// with. Landing the parameter with no behaviour change is deliberate --
    /// the compiler forces all seven seams to state their own posture
    /// before any task actually branches on it.
    #[test]
    fn launch_mode_names_the_two_postures_the_projection_splits_on() {
        assert_eq!(LaunchMode::Interactive.label(), "interactive");
        assert_eq!(LaunchMode::Headless.label(), "headless");
        assert!(LaunchMode::Interactive.is_interactive());
        assert!(!LaunchMode::Headless.is_interactive());
    }

    fn flags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn last_model_flag_reads_the_separated_short_form() {
        assert_eq!(last_model_flag(&flags(&["-m", "opus"])), Some("opus"));
    }

    #[test]
    fn last_model_flag_reads_the_joined_equals_short_form() {
        assert_eq!(last_model_flag(&flags(&["-m=opus"])), Some("opus"));
    }

    #[test]
    fn last_model_flag_reads_the_attached_short_form() {
        assert_eq!(last_model_flag(&flags(&["-mopus"])), Some("opus"));
    }

    /// Last occurrence wins across every mixed spelling -- long, short
    /// separated, short joined, short attached -- in argv order.
    #[test]
    fn last_model_flag_last_wins_across_mixed_forms() {
        assert_eq!(
            last_model_flag(&flags(&["--model", "opus", "-mhaiku"])),
            Some("haiku"),
            "a later attached -m overrides an earlier long --model"
        );
        assert_eq!(
            last_model_flag(&flags(&["-mopus", "--model=sonnet"])),
            Some("sonnet"),
            "a later joined --model= overrides an earlier attached -m"
        );
        assert_eq!(
            last_model_flag(&flags(&["-m", "opus", "-m=haiku", "-msonnet"])),
            Some("sonnet"),
            "every short form in argv order, last wins"
        );
    }

    /// `--model-foo` starts with `-m` once its own leading `-` is peeled,
    /// but it is a `--`-prefixed long flag, not codex's short alias, and
    /// must never be misread as `-m` with an attached value of `odel-foo`.
    #[test]
    fn a_long_flag_that_merely_starts_with_m_does_not_match() {
        assert_eq!(last_model_flag(&flags(&["--model-foo", "opus"])), None);
    }

    /// A bare `-m` with nothing after it (end of args) has no value to
    /// contribute -- it must not be read as naming an empty/wrong model, and
    /// must not clear an earlier real match either.
    #[test]
    fn a_trailing_bare_short_flag_with_no_value_contributes_nothing() {
        assert_eq!(last_model_flag(&flags(&["-m"])), None);
        assert_eq!(
            last_model_flag(&flags(&["-m", "opus", "-m"])),
            Some("opus"),
            "a later dangling -m must not erase the earlier real match"
        );
    }

    #[test]
    fn last_model_flag_returns_none_with_no_model_flag_at_all() {
        assert_eq!(last_model_flag(&flags(&["--verbose", "-x"])), None);
    }

    // `model_only_flags`: the one trailing-flag shape a dashboard pane can
    // honour, in every spelling `classify_model_flag` reads.

    #[test]
    fn model_only_flags_reads_every_spelling_of_a_lone_model_pin() {
        for spelling in [
            vec!["--model", "haiku"],
            vec!["--model=haiku"],
            vec!["-m", "haiku"],
            vec!["-m=haiku"],
            vec!["-mhaiku"],
        ] {
            assert_eq!(
                model_only_flags(&flags(&spelling)),
                Some("haiku"),
                "{spelling:?} pins a model and nothing else"
            );
        }
    }

    /// Anything beyond a model pin means the pane cannot honour what the
    /// operator typed, so the delegation goes headless instead of silently
    /// dropping the rest.
    #[test]
    fn model_only_flags_rejects_flags_a_pane_cannot_honour() {
        for other in [
            vec![],
            vec!["--verbose"],
            vec!["--model", "haiku", "--verbose"],
            vec!["--dangerously-skip-permissions", "--model=haiku"],
        ] {
            assert_eq!(
                model_only_flags(&flags(&other)),
                None,
                "{other:?} is not a lone model pin"
            );
        }
    }

    /// A pin with no usable value is not a pin: a dangling bare flag, a blank
    /// value, and a flag-shaped value all decline the pane rather than build a
    /// `--model` argv token out of nonsense.
    #[test]
    fn model_only_flags_rejects_a_pin_with_no_usable_value() {
        assert_eq!(model_only_flags(&flags(&["--model"])), None);
        assert_eq!(model_only_flags(&flags(&["--model", "  "])), None);
        assert_eq!(model_only_flags(&flags(&["--model="])), None);
        assert_eq!(model_only_flags(&flags(&["--model", "--verbose"])), None);
    }

    // `AgentAdapter::policy_args`: one `EffectivePolicy` input, equivalent
    // real-launch restriction on both registered adapters (Bug B).

    /// Unlike `seat_model_env` (orchestrator-only), `seat_role_env` fires
    /// for every role -- a hook process needs to tell a worker or
    /// sub-orchestrator seat apart from an orchestrator one just as
    /// reliably as it needs to detect an orchestrator seat at all.
    #[test]
    fn seat_role_env_labels_every_role() {
        use crate::commands::ctx::prompt::PromptRole;

        assert_eq!(
            seat_role_env(PromptRole::Orchestrator),
            vec![(SEAT_ROLE_ENV.to_string(), "orchestrator".to_string())]
        );
        assert_eq!(
            seat_role_env(PromptRole::SubOrchestrator),
            vec![(SEAT_ROLE_ENV.to_string(), "sub-orchestrator".to_string())]
        );
        assert_eq!(
            seat_role_env(PromptRole::Worker),
            vec![(SEAT_ROLE_ENV.to_string(), "worker".to_string())]
        );
        // Issue #537 (T3): the proxy's own single seat labels identically to
        // every other non-orchestrator role -- only the literal
        // `"orchestrator"` ever gates a guard keyed on this env var.
        assert_eq!(
            seat_role_env(PromptRole::Single),
            vec![(SEAT_ROLE_ENV.to_string(), "single".to_string())]
        );
    }

    // -- issue #690: launch error formatting and presence-based default selection -
}
