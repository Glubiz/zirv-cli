use super::*;

/// Issue #427: named tiers for the Orchestrator-only meta-harness
/// orientation layer (`prompt::HARNESS_PROMPT`), each with its own pinned
/// byte budget (`prompt::harness_prompt_for`'s bloat-guard tests).
/// `Minimal` is the narrow end (fewest bytes, functional lines only),
/// `Verbose` is today's full text. `REPO_FORBIDDEN` outright (see that
/// entry's own comment) rather than a narrow-only fold like
/// `OrchestratorWrites`, so no `PartialOrd`/`Ord` is needed here: a repo
/// layer may never set this key at all, in either direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptVerbosity {
    Minimal,
    Standard,
    #[default]
    Verbose,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub enabled: bool,
    /// Whether `<repo>/.zirv/system-prompt.md` is read at all.
    pub repo_layer: bool,
    /// Issue #427: how much of the Orchestrator-only meta-harness
    /// orientation layer (`prompt::HARNESS_PROMPT` and its tiered variants,
    /// selected by `prompt::harness_prompt_for`) is injected. `Minimal` is
    /// the narrow end (fewest bytes). `Verbose` reproduces today's
    /// `HARNESS_PROMPT` text byte for byte -- see that constant's own doc
    /// comment and `harness_prompt_for`'s bloat-guard tests for the pinned
    /// budget each tier stays under. `REPO_FORBIDDEN`, same trust asymmetry
    /// as `harnesses`/`codex_orchestrator` above: a repo checkout must not
    /// be able to raise the tier back up for an operator who chose a lower
    /// one.
    pub verbosity: PromptVerbosity,
    /// Cap on the repo layer only: untrusted text does not get to be long.
    pub max_repo_bytes: usize,
    /// Whether an Orchestrator session's composed prompt gets the derived
    /// harness roster (`adapters::harness_prompt_lines`, folded in by
    /// `prompt::compose` as `PromptSource::Harnesses`). On by default; an
    /// operator who wants no roster at all (or finds it noisy) turns it off
    /// here. `REPO_FORBIDDEN` (like `prompt.enabled`/`prompt.repo_layer`): a
    /// repo checkout must not be able to suppress a layer the operator
    /// relies on to see what this session may delegate to.
    pub harnesses: bool,
    /// Whether the standing skill index layer is composed at all; skills
    /// stay loadable through `zirv skill list`/`load` either way. NOT
    /// `REPO_FORBIDDEN`: a repo checkout may only narrow it to `false`
    /// (`narrow_skill_index_bool`), the same asymmetry `context.
    /// dedupe_native` uses, never force it back on for an operator who
    /// turned it off.
    pub skill_index: bool,
    /// Issue #753: whether the `UserPromptSubmit` hook classifies a
    /// session's FIRST prompt (text only, no network) and, for a
    /// substantial one, adds the one-turn plan/test discipline note
    /// (`hook::INTAKE_DISCIPLINE_TEXT`). NOT `REPO_FORBIDDEN`: a repo
    /// checkout may only narrow it to `false` (`narrow_intake_discipline_
    /// bool`), never force it back on for an operator who turned it off.
    pub intake_discipline: bool,
    /// Issue #755: whether the standing skill index drops a skill family
    /// the repository shows no signal for (`frontend-*` with no
    /// `package.json`/frontend source files; the four Kibana/Elastic
    /// operational skills with no Elastic/Kibana config) -- see
    /// `prompt::filter_skill_entries_by_repo_signal`. A dropped skill stays
    /// fully loadable through `zirv skill list`/`load`; only its passive
    /// advertisement in this layer narrows. `REPO_FORBIDDEN`, the same
    /// trust asymmetry as `harnesses`/`codex_orchestrator` above: disabling
    /// this heuristic widens what a session sees (every skill listed again),
    /// so only the operator may do it -- a repo checkout must not be able to
    /// force its own family back into every session's standing prefix.
    pub skill_index_repo_filter: bool,
    /// Whether a codex Orchestrator session's composed prompt gets codex's
    /// own `AgentAdapter::base_system_prompt` layer (issue #167,
    /// `adapters::codex::ORCHESTRATOR_PROMPT`) -- the codex analogue of
    /// claude's `ORCHESTRATOR_PROMPT`, spliced in by `prompt::with_adapter_
    /// layer`. On by default; an operator who finds it redundant with their
    /// own AGENTS.md conventions turns it off here. `REPO_FORBIDDEN`, same
    /// trust asymmetry as `harnesses` right above: a repo checkout must not
    /// be able to force this layer back on for an operator who turned it
    /// off. Claude's own orchestrator layer has no such switch -- it is not
    /// operator-toggleable independent of `prompt.enabled` -- so this key
    /// only ever gates the codex adapter.
    pub codex_orchestrator: bool,
    /// This seat's own repository-write guard posture (`SuperviseConfig::
    /// orchestrator_writes`, `[supervise] orchestrator_writes`) -- NOT a
    /// `[prompt]` TOML key of its own: `#[serde(skip)]` means a `[prompt]
    /// orchestrator_writes = ...` in either config layer hard-errors as an
    /// unknown field under this struct's own `deny_unknown_fields` rather
    /// than silently taking effect from the wrong section. `CtxConfig::load`
    /// copies this over from `supervise` right after the full config is
    /// assembled, purely so `prompt::with_adapter_layer` -- which already
    /// threads `&PromptConfig` through every launch site (`wrap`, `exec`,
    /// `run_loop`, `resume`, `chat`) -- can read this seat's write posture
    /// without a new parameter threaded through six more call sites.
    #[serde(skip)]
    pub orchestrator_writes: OrchestratorWrites,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repo_layer: true,
            verbosity: PromptVerbosity::Verbose,
            max_repo_bytes: 4096,
            harnesses: true,
            skill_index: true,
            intake_discipline: true,
            skill_index_repo_filter: true,
            codex_orchestrator: true,
            orchestrator_writes: OrchestratorWrites::Advise,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Cap on the canonical `.zirv/context/common.md` layer (issue #44's
    /// context compiler, `compile.rs`). Same rationale as `prompt.max_repo_
    /// bytes`: untrusted repo text does not get to be long, and the cap
    /// would be decorative if a repo checkout could simply raise its own
    /// limit -- see `REPO_FORBIDDEN`.
    pub max_common_bytes: usize,
    /// Cap on the canonical harness-specific addition
    /// (`.zirv/context/claude.md` / `.zirv/context/codex.md`), applied
    /// independently of `max_common_bytes` since the two files are read and
    /// truncated separately.
    pub max_harness_bytes: usize,
    /// Issue #46 ("Context 8/8"): the one layer `compile.rs`'s composed
    /// prompt injects with no budget at all before this -- an Orchestrator
    /// session's derived harness roster (`adapters::harness_prompt_lines`,
    /// folded in as `PromptSource::Harnesses`). Every other layer already had
    /// a configured cap (`prompt.max_repo_bytes`, `context.max_common_bytes`/
    /// `max_harness_bytes`, `mail.max_delivered_bytes`, `memory.max_injected_
    /// bytes`); this closes the gap the same way: enforced by `prompt::
    /// compose` itself (`prompt::harness_roster_injection`, truncated with
    /// `crate::utils::truncate_bytes` exactly like every layer above), not
    /// merely reported against by `zirv context status`. Same trust
    /// rationale as `max_common_bytes`/`max_harness_bytes` above -- see
    /// `REPO_FORBIDDEN`.
    pub max_harness_roster_bytes: usize,
    /// Issue #155, Phase 3: skip injecting the canonical `.zirv/context/`
    /// layer when the harness's own native instruction file
    /// (`<repo>/CLAUDE.md` for claude, `<repo>/AGENTS.md` for codex) is a
    /// zirv-managed render that PROVABLY holds the current canonical bytes
    /// -- proven by the hash `context_cli::render_generated` stamps into it,
    /// never assumed. The harness reads that file natively at session
    /// start, so injecting the same bytes again is pure duplication in the
    /// single most cacheable layer there is.
    ///
    /// Deliberately NOT `REPO_FORBIDDEN`, unlike the byte caps above it: a
    /// repo layer can only ever set it `false`, and `false` injects MORE
    /// context, which is narrowing. `CtxConfig::load` folds it with
    /// `narrow_dedupe_bool` (the mirror of `narrow_pace_bool`, but with
    /// `false` as the strict value instead of `true` -- this key's safe
    /// direction is "inject more", the opposite polarity from
    /// `pace.enabled`'s "gate is on"), so a repo `true` cannot re-enable a
    /// skip the operator turned off.
    pub dedupe_native: bool,
    /// Issue #275 (`zirv context lint`): a hard ceiling on how many sentence
    /// pairs the duplicate (CTX002) and contradiction-candidate (CTX003)
    /// checks will ever compare across every layer combined. Both checks are
    /// pairwise over the imperative sentences they collect, so cost grows
    /// quadratically with the amount of instructional prose across every
    /// canonical/native layer -- this bounds that, the same "untrusted repo
    /// text does not get to be unbounded" rationale as `max_common_bytes`
    /// above, except the resource here is CPU time during `zirv context
    /// lint`, not injected bytes. Exceeding the cap does not fail the lint:
    /// `context_lint::analyze` stops comparing once it is spent and reports
    /// `degraded: true` instead, so a very large repository still gets a
    /// (partial) report rather than a hang. `REPO_FORBIDDEN`: a repo layer
    /// could otherwise raise its own cap to force an expensive comparison
    /// an operator deliberately bounded, the same asymmetry as every other
    /// numeric key in this struct.
    pub lint_max_pairs: usize,
    /// Issue #538 (chunk B): aggregate byte budget for the native
    /// compiler's whole instruction layer -- every resolved `ZIRV.md`/
    /// `AGENTS.md`/`CLAUDE.md`/`AGENT.md` winner for the repo root and the
    /// active scope's ancestor chain, combined. Applied in stable order
    /// (root first, then nested by depth) after the per-file cap
    /// (`optimize.max_surface_bytes`, reused unchanged) already bounds any
    /// one file: this is the ceiling on the layer as a whole, so a
    /// monorepo with many small nested files cannot still blow the budget
    /// through sheer count. `REPO_FORBIDDEN`, same rationale as
    /// `max_common_bytes` above -- see `REPO_FORBIDDEN`.
    pub instructions_max_bytes: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_common_bytes: 4096,
            max_harness_bytes: 4096,
            max_harness_roster_bytes: 4096,
            dedupe_native: true,
            lint_max_pairs: 20_000,
            instructions_max_bytes: 32 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MailConfig {
    pub enabled: bool,
    /// Cap on a stored message's body. Enforced by `mail::store`, which
    /// truncates rather than fails an oversize message.
    pub max_message_bytes: usize,
    /// Cap on how much mail is surfaced to a session at once (delivery is a
    /// later piece; the cap lives here so it is configured alongside the
    /// rest of the mailbox from the start).
    pub max_delivered_bytes: usize,
    /// How many unread messages a repo's mailbox keeps before the oldest are
    /// pruned. Read messages, already moved into `read/`, are never touched
    /// by this limit.
    pub keep: usize,
}

impl Default for MailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_message_bytes: 4096,
            max_delivered_bytes: 4096,
            keep: 50,
        }
    }
}

/// Deploy policy embedded under `[workflow.deploy]`. `tier` is operator-only;
/// `minimum_tier` is the single repository-controlled workflow key and may
/// only ratchet strictness upward during layered config resolution.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowDeployConfig {
    pub tier: crate::commands::workflow::deploy::DeployTier,
    pub minimum_tier: Option<crate::commands::workflow::deploy::DeployTier>,
}

impl Default for WorkflowDeployConfig {
    fn default() -> Self {
        Self {
            tier: crate::commands::workflow::deploy::DeployTier::Development,
            minimum_tier: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaintainDetectorMode {
    #[default]
    ExitNonzero,
    LineCount,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintainDetectorConfig {
    pub command: String,
    pub mode: MaintainDetectorMode,
    pub threshold: u64,
}

impl Default for MaintainDetectorConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            mode: MaintainDetectorMode::ExitNonzero,
            threshold: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowMaintainConfig {
    pub timeout_secs: u64,
    pub detectors: std::collections::BTreeMap<String, MaintainDetectorConfig>,
}

impl Default for WorkflowMaintainConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 60,
            detectors: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReportConfig {
    /// GitHub owner/repository used by workflow maintenance incident filing.
    /// The ordinary `zirv report` command intentionally keeps its product
    /// default and does not consume this destination.
    pub repository: Option<String>,
}

/// Issue #315 (`zirv ctx search`): the single knob for how much rendered
/// text a search/scroll call is allowed to hand back at once, the same
/// "untrusted/large content does not get to be unbounded" rationale as
/// `context.max_common_bytes`/`mail.max_delivered_bytes` above -- a
/// mid-task agent calls this to check its own history without spending a
/// model call, and an unbounded result would defeat that purpose by
/// flooding the calling session's context instead. `REPO_FORBIDDEN`: a
/// checked-out repo raising its own output cap is exactly the same
/// asymmetry as every other byte cap in this file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Hard cap, in bytes, on one rendered window (`search::render_window_
    /// text`). Default `2048`, matching the issue's own acceptance
    /// criterion.
    pub max_output_bytes: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: 2048,
        }
    }
}

/// Issue #326: the compact-output knobs -- whether claude's `PostToolUse`
/// hook replaces a verbose Bash tool result with a summary at all, how big a
/// result has to be before that is worth doing, and the hard ceiling on the
/// summary itself.
///
/// `compact`, `compact_min_bytes`, `compact_generic_min_bytes`, `verbatim`
/// (additive-only) and `max_summary_bytes` are all `REPO_FORBIDDEN`.
/// `max_summary_bytes` is the same trust asymmetry as every other byte cap in
/// this file: a checked-out repository raising the cap on text that lands
/// directly in a supervised session's context window is exactly the flooding
/// these caps exist to prevent. `compact` and `compact_min_bytes` are
/// forbidden in BOTH directions, unlike the narrowing-only switches
/// elsewhere: turning compaction *on* lets a repository decide that what its
/// own build prints reaches the session only through a summary zirv wrote,
/// and turning it *off* (or raising the threshold past anything it ever
/// emits) lets a repository that floods on purpose opt itself out of being
/// compacted. Neither is the checkout's call.
///
/// `diff_max_bytes` (issue #412) is different: narrow-only, the same
/// "repo may only make it stricter" shape as `supervise.loop_backoff_ceiling_
/// secs` (see `narrow_diff_max_bytes`) -- a repo checkout may lower the size
/// at which its own `git diff`/`show`/`log -p`/`format-patch` output gets
/// replaced by a bounded per-file listing, never raise it past the
/// operator's own ceiling.
///
/// `filter` (issue #417) is `REPO_FORBIDDEN` as a whole table
/// (`~/.zirv/ctx.toml only` -- there is no `ZIRV_CTX_*` escape hatch for a
/// structured rule list): a repo checkout choosing how its own output gets
/// shaped once it is already large enough to summarize is the identical
/// widening `compact`/`compact_min_bytes`/`compact_generic_min_bytes` above
/// are already forbidden from doing, just one layer further in. `filter_
/// defaults` is `REPO_FORBIDDEN` the same way `compact`/`compact_search`
/// are, in both directions: it gates whether zirv's own bundled `[[output.
/// filter]]` rules (`output_filters::bundled_output_filter_rules`) join the
/// operator's own `filter` list at all.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Whether claude's `PostToolUse` hook (`hook::run_posttool`) replaces a
    /// large `Bash` tool result with a compact summary before the model ever
    /// sees it. Default `true`. `zirv ctx run --compact` is unaffected: it is
    /// an explicit invocation, not an interception.
    pub compact: bool,
    /// How many bytes of combined stdout+stderr a result from a KNOWN
    /// build/test/log family (`output::classify_compaction`) needs before the
    /// `PostToolUse` hook compacts it. Below this the original output is left
    /// exactly as it is -- compacting a short result spends a stored file and
    /// a retrieval round trip to save nothing. Default `4096`.
    pub compact_min_bytes: usize,
    /// The same threshold for a command whose output shape zirv does NOT
    /// recognise. Deliberately much higher than `compact_min_bytes`: for a
    /// known `cargo test` the summary provably keeps the lines that matter,
    /// while for an unrecognised producer a head/tail is a guess, so the
    /// output has to be genuinely large before that guess is worth making.
    /// Default `16384`.
    pub compact_generic_min_bytes: usize,
    /// Extra program names that are NEVER compacted, added to
    /// `output::VERBATIM_PROGRAMS`. Operator-only and purely ADDITIVE -- it
    /// can only ever protect more output, never less. An operator naming
    /// their own pager, formatter or dump tool here is telling zirv that a
    /// model reads that command's output verbatim before acting on it, which
    /// a head/tail summary would silently corrupt. Empty by default.
    pub verbatim: Vec<String>,
    /// Hard cap, in bytes, on one compact summary
    /// (`output::render_summary`). Absolute: a summary whose MANDATORY
    /// failure content does not fit inside it is not emitted at all (the
    /// original output is used instead) rather than emitted missing
    /// failures. Default `4096`; [`MIN_MAX_SUMMARY_BYTES`] is the floor
    /// `CtxConfig::load` enforces.
    pub max_summary_bytes: usize,
    /// Bytes of combined stdout+stderr a `git diff`/`show`/`log -p`/
    /// `format-patch` result (`output::CompactionScope::Diff`) may reach
    /// before the `PostToolUse` hook replaces it with a bounded per-file
    /// listing (`output_diff::render_diff_summary`) instead of a head/tail --
    /// never a partial hunk, since a diff's middle is exactly the part a
    /// model is about to edit against. Narrow-only, protective like
    /// `supervise.loop_backoff_ceiling_secs`: a repo checkout may LOWER this,
    /// never raise it past the operator's own ceiling (see
    /// `narrow_diff_max_bytes`). Generous default `65536` (64 KiB): most
    /// diffs a supervised session produces never reach it.
    pub diff_max_bytes: usize,
    /// Issue #414: whether `rg`/`grep`/`find`/`fd`/`ls`/`dir`/`tree` (all
    /// `output::VERBATIM_PROGRAMS` members) get a shape-aware pass
    /// (`output_search`) instead of staying verbatim at any size. Forbidden
    /// in BOTH directions like `compact`/`compact_min_bytes`/
    /// `compact_generic_min_bytes` above, never narrow-only: turning it off
    /// WIDENS what a repository checkout's own commands get compacted into
    /// just as much as turning it on would, which is exactly the choice
    /// those two keys' own doc comment says is never the checkout's to
    /// make. Default `true` -- the original is always retrievable with
    /// `zirv ctx output show <id>`; an operator turns this off with
    /// `compact_search = false` in `~/.zirv/ctx.toml` or
    /// `ZIRV_CTX_OUTPUT_COMPACT_SEARCH=false`.
    pub compact_search: bool,
    /// Issue #417: an operator-declared rule list that shapes a `Generic`-
    /// scope command's output BEFORE the generic head/tail scan
    /// (`output::render_summary`) ever sees it -- declared in
    /// `~/.zirv/ctx.toml` only, never settable from a repo checkout (see
    /// `REPO_FORBIDDEN`'s `output.filter` entry: a checkout choosing what
    /// its own output looks like once summarized is exactly the widening
    /// every other `[output]` key in this file already refuses). Applies to
    /// `CompactionScope::Generic` only -- never `Verbatim`, `Known`, `Diff`
    /// or `Shape`, each of which already has its own dedicated,
    /// provably-lossless rendering. The first rule (in declaration order)
    /// whose `match_command` matches the command line wins; every other
    /// rule is ignored for that command. Empty by construction here --
    /// `CtxConfig::load` appends `output_filters::bundled_output_filter_
    /// rules` (unless `filter_defaults` is `false`), after every operator
    /// rule and skipping any bundled rule whose `name` an operator rule
    /// already used. See `OutputFilterRule`'s own doc comment for the field
    /// list and stage order.
    pub filter: Vec<OutputFilterRule>,
    /// Issue #(bundled defaults): whether `CtxConfig::load` appends zirv's
    /// own bundled `[[output.filter]]` rules (`output_filters::
    /// bundled_output_filter_rules`) after the operator's own `filter`
    /// entries, so compaction shapes common noisy build/install/download
    /// tool output out of the box with zero configuration. `false` yields
    /// only the operator's own rules. `REPO_FORBIDDEN` in BOTH directions,
    /// the same shape as `compact`/`compact_search` above: a repo checkout
    /// must not be able to widen its own output shaping by re-enabling
    /// defaults an operator turned off, nor narrow it by turning off
    /// defaults an operator wants applied everywhere. Default `true`.
    pub filter_defaults: bool,
}

/// One `[[output.filter]]` rule (issue #417). `name` and `match_command` are
/// mandatory -- every load error names the rule by `name`, and a rule with
/// no way to select a command would silently apply to everything. Every
/// other field is optional and does nothing when absent/empty.
///
/// Stages run in this FIXED order, documented again on
/// `output::apply_output_filter_stages` (the code that actually applies
/// them): (1) `match_output` -- whole-output, short-circuits every other
/// stage below when it fires; (2) `strip_lines` -- drop every line matching
/// any pattern; (3) `keep_lines` -- when non-empty, drop every line NOT
/// matching any pattern; (4) `truncate_line_at` -- cut each surviving line
/// to at most this many *characters* (never bytes, so a multi-byte character
/// is never split); (5) `max_lines` -- keep only the first N lines and
/// append a marker naming how many more there were and where to retrieve
/// them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputFilterRule {
    /// Human-readable identity for this rule, used in every load-time error
    /// message and nowhere else. Mandatory: an anonymous rule would leave an
    /// operator unable to tell which one of several a load error names.
    pub name: String,
    /// A `regex::Regex` pattern matched against the command line
    /// (`command.join(" ")`, the same string `hook::run_posttool` and
    /// `run --compact` already build) to decide whether this rule applies at
    /// all. Mandatory, and validated at load time (`CtxConfig::load`) two
    /// ways: it must compile, and it must be FULLY ANCHORED -- every
    /// top-level `|` alternative starts with `^` (`is_fully_anchored`) -- so
    /// `gradle` (which would match `my-not-gradle-thing`) is refused, while
    /// `^(\\./)?gradlew?\\b` is accepted.
    pub match_command: String,
    /// Regex patterns (`regex::Regex`, matched per line, unanchored is fine
    /// here -- only `match_command` needs anchoring): a line matching ANY of
    /// these is dropped before `keep_lines` even runs. Empty (the default)
    /// drops nothing.
    #[serde(default)]
    pub strip_lines: Vec<String>,
    /// Regex patterns: when this list is non-empty, only a line matching AT
    /// LEAST ONE of them survives -- everything else is dropped, on top of
    /// whatever `strip_lines` already removed. Empty (the default) keeps
    /// every surviving line.
    #[serde(default)]
    pub keep_lines: Vec<String>,
    /// Cuts each surviving line to at most this many *characters* (built
    /// with `char_indices`, never a byte slice, so a multi-byte character is
    /// never split mid-codepoint). `None`/absent (the default) truncates
    /// nothing.
    #[serde(default)]
    pub truncate_line_at: Option<usize>,
    /// Keeps only the first N surviving lines and appends one marker line
    /// naming how many more there were and how to retrieve the untouched
    /// original (`zirv ctx output show <id>`). `None`/absent (the default)
    /// never truncates by line count.
    #[serde(default)]
    pub max_lines: Option<usize>,
    /// Whole-output short-circuit: when `pattern` matches the ENTIRE
    /// original text (not merely a substring of it -- the match span must
    /// cover the whole input), the result is `replace` outright and no other
    /// stage in this rule runs. `None`/absent (the default) never fires.
    #[serde(default)]
    pub match_output: Option<MatchOutput>,
}

/// `[[output.filter]]`'s whole-output short-circuit (see `OutputFilterRule::
/// match_output`'s own doc comment for exactly when it fires).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOutput {
    /// A `regex::Regex` pattern; validated at load time the same way
    /// `match_command` is (must compile), but NOT required to be anchored --
    /// the whole-output-match check at apply time already requires the
    /// match span to cover the entire text, which is a stronger constraint
    /// than anchoring alone.
    pub pattern: String,
    /// The literal text substituted in when `pattern` matches the whole
    /// output. No capture-group interpolation -- a fixed replacement only.
    pub replace: String,
}

/// The smallest `[output] max_summary_bytes` that can hold a header, a
/// failure line and the retrieval line without the cap eating into the
/// content the summary exists for. A lower value is a hard config error
/// naming the key rather than a silent clamp, so an operator who asks for 64
/// learns what they actually asked for.
pub const MIN_MAX_SUMMARY_BYTES: usize = 512;

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            compact: true,
            compact_min_bytes: 4096,
            compact_generic_min_bytes: 16384,
            verbatim: Vec::new(),
            max_summary_bytes: 4096,
            diff_max_bytes: 65536,
            compact_search: true,
            filter: Vec::new(),
            filter_defaults: true,
        }
    }
}

/// Issue #264: the cost ledger's own pricing knobs. Both fields are
/// `REPO_FORBIDDEN` -- a repo checkout must not be able to widen how long a
/// stale price table is presented as trustworthy, or point pricing at a file
/// of its own choosing (see `price::PriceTable`/`price::price`, and
/// [[Untrusted Configuration]]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PriceConfig {
    /// How many days old a price table's own `as_of` stamp may be before
    /// every cost line it prices renders `~$x (prices as of …)` instead of a
    /// plain figure -- a stale table is silently wrong money, never a plain
    /// number (`price::PriceTable::is_stale`).
    pub stale_after_days: u64,
    /// Operator override path for the price table, read and merged over the
    /// built-in one the same way `~/.zirv/prices.toml` is when present.
    /// `None` -- the default -- is that ordinary resolution
    /// (`price::resolve_table`); set only to point at a NON-default location.
    pub table_path: Option<String>,
}

impl Default for PriceConfig {
    fn default() -> Self {
        Self {
            stale_after_days: 90,
            table_path: None,
        }
    }
}

/// Issue #312: thresholds for `hook.rs`'s reclaim-gated compact advisory --
/// a SECOND, cost-driven tier alongside the rot `Verdict` ladder, firing only
/// when stale tool-result tokens exceed `min_reclaim_tokens` AND the window
/// exceeds `window_fraction` of the model's context window.
///
/// Deliberately **not** `REPO_FORBIDDEN`, unlike `score.token_floor`/
/// `token_ceiling` right above: those gate the rot engine's own
/// restart/compact behavior, so a checkout that could widen them escapes
/// real supervision. This section only tunes how eagerly a PURE, ignorable
/// suggestion fires. It is still narrow-only, the same shape as
/// `[verify_on_stop]`/`[diagnostics]`: a repo layer may only RAISE either
/// threshold (quieten the advisory), never lower one so the Stop hook nags
/// every turn -- see `narrow_compact_advisory_min_reclaim`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactAdvisoryConfig {
    /// Stale tool-result tokens (the `tool_results_stale` bucket of
    /// `breakdown::BreakdownSummary`) must exceed this before the advisory
    /// even considers firing. Default `4096`.
    pub min_reclaim_tokens: u64,
    /// AND the current window must exceed this fraction of the model's
    /// resolved context window (the same capacity `rot::token_gates`
    /// resolves) before the advisory fires. Default `0.6`.
    pub window_fraction: f64,
}

impl Default for CompactAdvisoryConfig {
    fn default() -> Self {
        Self {
            min_reclaim_tokens: 4096,
            window_fraction: 0.6,
        }
    }
}

/// Operator-controlled switches over the workflow subsystem
/// (`src/commands/workflow/`). Every field is repo-forbidden except the
/// explicitly folded `workflow.deploy.minimum_tier`, which can only make the
/// effective deploy tier stricter.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowConfig {
    /// Whether repository-supplied verification checks may actually run.
    /// When false they are still listed in the report, with a skip line, so
    /// an operator can see what the repo asked for without running it.
    pub repo_checks_enabled: bool,
    /// Whether `.zirv/skills/` manifests are loaded at all. Repository
    /// skills can only ever *add* ids (see `skill::load_dir`); this turns the
    /// whole layer off.
    pub repo_skills_enabled: bool,
    /// Whether untrusted repository-provided `.zirv/agents/` manifests are
    /// loaded. Off by default: a checkout may propose a new role only after
    /// the operator explicitly enables this layer, and may never replace a
    /// trusted built-in/operator id.
    pub repo_agents_enabled: bool,
    /// Whether untrusted repository-provided `.zirv/workflows/` definition
    /// packs (`WorkflowDefinitionV2`, issue #542) are loaded at all. Off by
    /// default, same posture as `repo_agents_enabled`: a checkout may
    /// propose a new pack only after the operator explicitly enables this
    /// layer, and a loaded pack may still never replace a trusted built-
    /// in/operator id or widen authority -- see `workflow::registry::
    /// WorkflowRegistry`.
    pub repo_workflows_enabled: bool,
    pub deploy: WorkflowDeployConfig,
    pub maintain: WorkflowMaintainConfig,
    /// Local workflow telemetry. Previously read straight from the process
    /// environment, which a repository script could set for itself.
    pub telemetry_enabled: bool,
    pub telemetry_max_events: usize,
    pub telemetry_retention_days: u64,
    /// Issue #223: how hard zirv pushes a session that has done substantial
    /// edit work with no active `zirv workflow` toward starting one. A
    /// checkout must not be able to loosen or tighten this for itself --
    /// same trust boundary as `deploy.tier`, minus the repo narrowing
    /// carve-out `deploy.minimum_tier` gets, since there is no direction here
    /// a repo may safely push.
    pub adoption: crate::commands::workflow::adoption::AdoptionPolicy,
    /// Extra environment variable names, read from zirv's own process
    /// environment at check-run time and set on a verification check child
    /// (`workflow::verification::run_check`) -- ADDED to that function's own
    /// built-in `DEFAULT_CHECK_ENV_PASSTHROUGH` (the SSH-agent family plus
    /// GPG's terminal/homedir pointers), never a replacement for it. Issue
    /// #233: an operator whose check toolchain needs a variable outside that
    /// default set (a corporate proxy token, say) names it here instead of
    /// prefixing every `verify.toml` command with a shell workaround.
    /// Empty by default. `REPO_FORBIDDEN`: the untrusted repo checkout that
    /// owns `verify.toml` must never be able to widen what its own checks
    /// can read from the operator's environment.
    pub check_env_passthrough: Vec<String>,
    /// `--budget-tokens` appended to a reviewer worker's launch when set.
    /// `REPO_FORBIDDEN`: operator-only, like `check_env_passthrough` above.
    pub review_worker_budget_tokens: Option<u64>,
    /// `--max-tool-calls` appended to a reviewer worker's launch when set.
    /// `REPO_FORBIDDEN`, same reasoning as `review_worker_budget_tokens`.
    pub review_worker_max_tool_calls: Option<u32>,
    /// Issue #242: auto-spawns a bounded `review run`/`test changed`/
    /// `verify` worker when a gate transition lands the workflow on that
    /// phase. Off by default. `REPO_FORBIDDEN`: a repo checkout must not be
    /// able to make zirv spend on its own behalf.
    pub auto_spawn_on_gate: bool,
    /// Issue #268: lets `zirv verify`/`zirv test` report `Passed` instead of
    /// `Inconclusive` when zero verification checks are configured or
    /// discoverable, rather than the degraded-gate ban's default of
    /// treating "nothing to check" as proving nothing. Off by default.
    /// `REPO_FORBIDDEN`: an untrusted checkout must not be able to declare
    /// its own missing/empty `verify.toml` a pass.
    pub allow_empty_verify: bool,
    /// Issue #276: `zirv verify`'s built-in self-check registry
    /// (`workflow::checks`) runs every registered id unless its dotted name
    /// is listed here. `REPO_FORBIDDEN`, same reasoning as
    /// `check_env_passthrough` above: the untrusted checkout these checks
    /// exist to police (adapter argv shape, `REPO_FORBIDDEN` widening,
    /// doc-verb drift, ...) must never be the one that turns them off.
    /// Empty by default, so every builtin runs.
    pub builtin_checks_exclude: Vec<String>,
    /// Issue #326: byte cap on the active workflow step's resolved skill
    /// instructions -- `engine::render_current_context`'s own output, both
    /// injected into the session prompt (`prompt::with_workflow_layer`) and
    /// printed directly by `zirv workflow context` -- so a step whose
    /// selected skills happen to be large does not inject them unbounded on
    /// every compose. A cut truncation still ends with a note naming the
    /// omitted byte count, never a silent cut. `REPO_FORBIDDEN`, same
    /// reasoning as `search.max_output_bytes`: a repo checkout must not be
    /// able to widen its own step-context output cap.
    pub max_context_bytes: usize,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            repo_checks_enabled: true,
            repo_skills_enabled: true,
            repo_agents_enabled: false,
            repo_workflows_enabled: false,
            deploy: WorkflowDeployConfig::default(),
            maintain: WorkflowMaintainConfig::default(),
            telemetry_enabled: true,
            telemetry_max_events: 1000,
            telemetry_retention_days: 30,
            adoption: crate::commands::workflow::adoption::AdoptionPolicy::default(),
            check_env_passthrough: Vec::new(),
            review_worker_budget_tokens: None,
            review_worker_max_tool_calls: None,
            auto_spawn_on_gate: false,
            allow_empty_verify: false,
            builtin_checks_exclude: Vec::new(),
            max_context_bytes: 8192,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// MASTER switch for the whole memory subsystem, kept under its
    /// original name for backward compatibility (it predates
    /// `memory::MemoryScope`). `false` disables the private, global, and
    /// shared scopes, however `shared_enabled` below is set. See
    /// `memory::MemoryScope::enabled`.
    pub enabled: bool,
    /// Whether facts may be harvested automatically from distilled handoffs.
    /// Off by default: an entry worth keeping across sessions is, for now, a
    /// deliberate act, not an inferred one.
    pub harvest: bool,
    /// How many entries a repository's bank keeps before the oldest
    /// (by `Written`) are pruned. Mirrors `mail.keep`.
    pub max_entries: usize,
    /// Cap on a single entry's body. Enforced by `memory::remember`, which
    /// truncates rather than fails an oversize entry.
    pub max_entry_bytes: usize,
    /// Superseded by `core_max_bytes` (issue #34): every prompt-injection
    /// call site now caps the merged private+global+shared core layer with that key
    /// instead. Kept, parsed, and still `REPO_FORBIDDEN`, purely so an
    /// existing `ctx.toml`/env setting this key does not hard-error on load
    /// (this struct is `deny_unknown_fields`) -- the same "kept under its
    /// original name" reasoning `enabled`'s own doc comment gives for a
    /// different field.
    pub max_injected_bytes: usize,
    /// Gate for the **shared** (repo-owned) memory bank under
    /// `<repo>/.zirv/memory/` (`memory::MemoryScope::Shared`), UNDERNEATH
    /// the master switch above: with `enabled = true`, an operator can turn
    /// this off to keep the private scope while dropping shared, but
    /// `enabled = false` always wins regardless of this value. On by
    /// default like every other memory switch.
    pub shared_enabled: bool,
    /// Hard byte budget for the **core** memory layer: private, global, and
    /// shared entries merged with private-first precedence (see
    /// `prompt::select_memory_within_cap`), always eligible for injection
    /// into every zirv-started session regardless of query or context.
    /// Independent of `max_entries`/`max_entry_bytes` (which cap what the
    /// bank *stores*) and of the bank's total size -- a strict ceiling on
    /// what a session actually *receives*.
    pub core_max_bytes: usize,
    /// Hard byte budget for the **retrieved** memory layer (issue #35):
    /// entries selected by context-aware ranking, added on top of the core
    /// layer for a query or session context. Independent of
    /// `core_max_bytes`.
    pub retrieval_max_bytes: usize,
    /// Hard cap on the *number* of entries the retrieval layer may select,
    /// independent of `retrieval_max_bytes`'s byte budget -- a ranking that
    /// matches many small entries must not still return dozens of them.
    pub retrieval_max_entries: usize,
    /// Issue #37: the most durable entries one session's own harvest
    /// (`memory::harvest_durable`, called at a rot/timeout restart or a
    /// clean session end) will store, regardless of how many candidates the
    /// model proposes -- a conservative per-session cap, independent of
    /// `init_max_entries`, which caps a whole-repository bootstrap batch
    /// instead of one session's own contribution.
    pub harvest_max_entries: usize,
    /// Issue #37: the cumulative byte budget for one session's own harvest,
    /// summed over every entry it stores -- independent of `max_entry_bytes`
    /// (which caps a single entry) and of `init_max_bytes` (which caps the
    /// bootstrap corpus sent to the model, not what gets written back).
    pub harvest_max_bytes: usize,
    /// Issue #295: gate for the **session** tier (`memory::MemoryScope::
    /// Session`, `<state>/memory/<repo_slug>/sessions/<session-id>/`),
    /// UNDERNEATH the master `enabled` switch above -- the same shape
    /// `shared_enabled` already has for the shared scope. On by default: a
    /// bare `zirv ctx remember`/`zirv memory remember` with a session id
    /// present writes to this tier instead of the private one unless this is
    /// turned off (or `--repo`/`--global` is given explicitly).
    pub session_enabled: bool,
    /// Issue #295: how many lines each memory bank's own `journal.jsonl`
    /// (one file per `<state>/memory/<repo_slug-or-_global>/`) keeps before
    /// the oldest are pruned, mirroring `max_entries`' own retention
    /// discipline for the entry bank itself. Independent of `max_entries`:
    /// a bank can hold fewer live entries than journal lines, since a
    /// `forget`/`verify`/`promote`/`rollback` each append a record without
    /// necessarily changing how many entries currently exist.
    pub journal_max_entries: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            harvest: false,
            max_entries: 50,
            max_entry_bytes: 512,
            max_injected_bytes: 2048,
            shared_enabled: true,
            core_max_bytes: 2048,
            retrieval_max_bytes: 2048,
            retrieval_max_entries: 6,
            harvest_max_entries: 5,
            harvest_max_bytes: 2048,
            session_enabled: true,
            journal_max_entries: 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    /// Issue #326 B1: byte budget on the combined `## PARENT OUTCOMES` block
    /// `task::compile_task_prompt` appends to a delegated worker's own
    /// prompt (`agent::attach_task_context_to_prompt`) -- every ancestor
    /// card's `outcome` used to be appended verbatim, uncapped, so a task
    /// tree a few levels deep could inject an unbounded amount of prior
    /// prose into a fresh worker's very first turn. The most recently
    /// updated parents are kept first and in full; once the budget is
    /// spent, the rest are dropped with an explicit `[truncated N bytes]`
    /// note naming how much was cut, never a silent one. `REPO_FORBIDDEN`
    /// (`ZIRV_CTX_TASK_MAX_PARENT_OUTCOME_BYTES`): without that, a repo
    /// checkout could simply raise its own cap, making it decorative, the
    /// same reasoning as `mail.max_delivered_bytes`/`memory.max_entry_bytes`.
    pub max_parent_outcome_bytes: usize,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_parent_outcome_bytes: 4096,
        }
    }
}

/// Issue #352: the persistent runtime service -- the one that owns PTYs so a
/// session survives the client that was looking at it. EXPERIMENTAL and
/// operator-only: with `persistent = false` (the default) nothing in this
/// table has any effect and every surface behaves exactly as it did before
/// the feature existed.
///
/// Every key here is `REPO_FORBIDDEN`. A checked-out repository must not be
/// able to decide that sessions started from it outlive the operator's
/// terminal, and -- the sharper half -- must not be able to turn on
/// `history`, which persists rendered terminal output (and therefore any
/// secret an agent happened to print) to disk. Only `~/.zirv/ctx.toml`, the
/// `ZIRV_CTX_SESSION_*` variables, or an explicit flag may set them.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// The master switch (`ZIRV_CTX_SESSION_PERSISTENT`). Off by default:
    /// until crash, upgrade and cross-platform recovery are proven on every
    /// platform, `zirv session serve` is something an operator opts into, and
    /// `zirv chat` keeps launching its own PTY in its own process.
    pub persistent: bool,
    /// Tier 3 (`ZIRV_CTX_SESSION_HISTORY`): persist each session's rendered
    /// terminal state across a RUNTIME restart, not just across a client
    /// detach. Off by default and warned about at the point of use, because
    /// terminal output routinely contains tokens, keys and repository
    /// contents, and this is the only zirv setting that writes that stream to
    /// disk. Detach/reattach (tier 1) does not need it: the original PTY and
    /// its live screen never left the service's memory.
    pub history: bool,
    /// How many rows of scrollback each server-owned PTY keeps in memory for
    /// a reattaching client (`ZIRV_CTX_SESSION_SCROLLBACK_ROWS`). Memory, not
    /// disk: unaffected by `history`.
    pub scrollback_rows: usize,
    /// How long a namespace record may go without a heartbeat before another
    /// process treats it as stale (`ZIRV_CTX_SESSION_STALE_AFTER_SECS`).
    /// Only ever a secondary signal: staleness is decided by process start
    /// identity first (see `session::namespace`), never by age or pid alone.
    pub stale_after_secs: u64,
}

/// Defaults are written out rather than derived so the "off by default"
/// promise is one visible line rather than an inference about `bool`.
impl SessionConfig {
    pub const DEFAULT_SCROLLBACK_ROWS: usize = 2000;
    pub const DEFAULT_STALE_AFTER_SECS: u64 = 120;

    /// The resolved scrollback budget, with `0` (an unset or explicitly
    /// zeroed key) reading as the built-in default rather than "keep
    /// nothing": the same clamping convention `setup.backup_retention_runs`
    /// and `workflow.telemetry_max_events` already use.
    pub fn scrollback_rows_or_default(&self) -> usize {
        if self.scrollback_rows == 0 {
            Self::DEFAULT_SCROLLBACK_ROWS
        } else {
            self.scrollback_rows
        }
    }

    pub fn stale_after_secs_or_default(&self) -> u64 {
        if self.stale_after_secs == 0 {
            Self::DEFAULT_STALE_AFTER_SECS
        } else {
            self.stale_after_secs
        }
    }
}

/// Bookkeeping for the guided `zirv setup` flow (issues #87, #93, #95). Not
/// `REPO_FORBIDDEN`: unlike the workflow/memory tables above, nothing here
/// gates execution of repository content or spend on the operator's
/// behalf -- `memory_harvest_offered`/`statusline_wrap_offered` only decide
/// whether `zirv setup` re-asks a question it already asked, and
/// `backup_retention_runs` only bounds local disk usage under
/// `.zirv/backups/ai-reset`. `setup.rs` is the only writer, and it writes
/// exclusively to the operator's own global `~/.zirv/ctx.toml`, never a
/// repo layer -- see `setup::set_home_ctx_toml_bool`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupConfig {
    /// Count cap on `.zirv/backups/ai-reset` runs, applied on the next
    /// backup write (never on `restore --list`, which stays read-only). The
    /// single oldest run is always pinned outside this cap -- see
    /// `setup::prune_backup_runs`. Clamped like `workflow.
    /// telemetry_max_events`: `0` keeps the built-in default, anything above
    /// the ceiling is clamped down.
    pub backup_retention_runs: usize,
    /// Set once the guided flow has asked whether to turn on automatic
    /// memory harvest, whichever way the operator answered -- so a decline
    /// is not re-asked on every `zirv setup` run.
    pub memory_harvest_offered: bool,
    /// Set once the guided flow has asked whether to wrap an existing
    /// custom Claude statusLine with zirv's usage tee.
    pub statusline_wrap_offered: bool,
}

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            backup_retention_runs: 20,
            memory_harvest_offered: false,
            statusline_wrap_offered: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChromeConfig {
    /// The launch banner naming the resolved harness, the rule that chose it,
    /// and the session id.
    pub banner: bool,
    /// The reserved bottom status bar (T12b).
    pub bar: bool,
    /// The `zirv ▸` announcement channel on stderr.
    pub events: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            banner: true,
            bar: true,
            events: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashConfig {
    pub enabled: bool,
    /// Width, in columns, of the persistent sidebar listing every session.
    /// The 2026-09-26 dash refresh (PR1) narrowed the approved default from
    /// 44 to 28 -- the width the row contract (`dash::ui`'s fixed columns
    /// plus a `name` field that widens) is drawn against at every terminal
    /// size. An explicit operator value is still honoured, and the layout
    /// still clamps whatever it is to the frame; below 100 total columns the
    /// sidebar hides altogether regardless of this value (see
    /// `dash::ui::layout`).
    pub sidebar_cols: u16,
    /// How long a quit-time roster stays offered for restore before a fresh
    /// launch treats it as stale and ignores it.
    pub roster_max_age_secs: u64,
    /// The most panes one dashboard will ever hold at once, counting the
    /// orchestrator. Defaults to 9, matching `DashAction::Switch`'s own
    /// `Ctrl+A 1..9` addressing: a pane nothing can select is a pane nobody
    /// asked for. Enforced wherever a pane is created from something other
    /// than the operator's own launch -- the spawn-request channel and the
    /// `Ctrl+A s` dialog -- so a pane child cannot fork-bomb its own
    /// dashboard into a machine full of harness processes.
    pub max_panes: usize,
    /// Whether the dashboard captures the mouse, which is what makes the
    /// wheel scroll a pane's scrollback.
    ///
    /// A toggle, and defaulted **on**, because it is a genuine trade rather
    /// than a strict improvement: a terminal that is reporting mouse events to
    /// the application no longer performs its own native click-drag text
    /// selection, so an operator who wants to select and copy text has to hold
    /// Shift to bypass the capture (the standard escape hatch every terminal
    /// offers, and the same trade tmux's own `mouse on` makes). Some operators
    /// live in the scrollback and some live in the selection; the wheel is the
    /// more discoverable of the two, so it wins the default, and anyone who
    /// disagrees sets `mouse = false` and still has `Ctrl+A PageUp`/`Home`.
    pub mouse: bool,
    /// How long, in milliseconds, a pane's pty output must have been quiet
    /// before a pane whose adapter has no turn-signal mechanism
    /// (`AgentAdapter::capabilities().turn_signal == false`, codex today) is
    /// treated as `Idle`. Such a pane never reports a turn boundary at all
    /// (`register_turn_signal` is a no-op for it), so `Pane::state`'s usual
    /// `signal_still_stands` gate -- which requires a signal to have been seen
    /// even once -- would leave it `Working` forever, and the mail sweep/nudge
    /// drain, both gated on `Idle`, would never fire into it. This is the
    /// output-quiescence fallback for that case only: a signal-carrying
    /// adapter's pane is untouched by this key, unchanged from before.
    ///
    /// Deliberately **not** `REPO_FORBIDDEN`, unlike every other key in this
    /// table: it is a pure timing/tuning knob over a session the operator
    /// already chose to run interactively in the dashboard, the same class of
    /// decision `pace.soft_percent` is (see that field's own doc comment) --
    /// not a cap standing between an untrusted layer and something it must not
    /// raise for itself (`dash.max_panes`), and not a switch over the
    /// operator's own terminal/machine (`dash.mouse`/`dash.sidebar_cols`).
    pub idle_quiet_ms: u64,
    /// Security review (2026-08-31, issue #228 follow-up): the roots a pane
    /// `--workdir` request (`spawnreq::SpawnRequest::workdir`) must
    /// canonicalise inside before `dash::mod::fulfill_spawn_request` will
    /// honour it. Without a confinement rule, `agent::validate_workdir`'s own
    /// check ("exists, is a directory, sits inside SOME git repository") lets
    /// a same-uid pane's forged request (issue #179's accepted threat model)
    /// obtain write authority over any repo checkout on the machine, not only
    /// ones the operator opened.
    ///
    /// The dashboard's own repo root and that root's parent directory are
    /// always roots, unconditionally -- a sibling checkout (`git worktree add
    /// ../other`, or a plain sibling clone) works with zero configuration,
    /// which is the feature's own use case (issue #228). This list is
    /// ADDITIONAL roots an operator opts into beyond those two, each an
    /// absolute path; a relative or nonexistent entry is kept literally
    /// (canonicalised best-effort, the same lenient fallback `same_directory`
    /// uses) rather than rejected outright, so a typo narrows rather than
    /// crashing the load.
    ///
    /// `REPO_FORBIDDEN`: a repo checkout must not be able to widen which
    /// directories a pane spawned from it may write into -- the exact
    /// privilege-widening asymmetry `sandbox.extra_allow` already holds for
    /// claude's own permission rules. `ZIRV_CTX_DASH_WORKDIR_ROOTS`
    /// (comma-separated, the same shape as `ZIRV_CTX_SANDBOX_EXTRA_ALLOW`)
    /// replaces the merged file value outright, the operator's own final
    /// word.
    pub workdir_roots: Vec<String>,
    /// Dash refresh PR2: `"full"` (default) or `"reduced"` -- see
    /// [`DashMotion`]'s own doc comment. Not `REPO_FORBIDDEN`, same
    /// reasoning as `idle_quiet_ms` right above: a presentation-only knob
    /// over a session the operator already chose to run interactively, not
    /// a cap standing between an untrusted layer and something it must not
    /// raise for itself.
    pub motion: DashMotion,
}

impl Default for DashConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sidebar_cols: 28,
            roster_max_age_secs: 604_800,
            max_panes: 9,
            mouse: true,
            idle_quiet_ms: 10_000,
            workdir_roots: Vec::new(),
            motion: DashMotion::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    /// Model for the interactive orchestrator session, passed through
    /// `AgentAdapter::model_args`. `None` leaves the agent's own default in
    /// place. Deliberately **not** in `REPO_FORBIDDEN` -- see the comment
    /// there and the spec's "Orchestrator model" section
    /// (docs/superpowers/specs/2026-08-13-zirv-dashboard-design.md): unlike
    /// `handoff.model`/`optimize.model`, this only shapes a session the
    /// operator deliberately launched interactively, and the choice is
    /// disclosed on screen at launch rather than spent silently in the
    /// background.
    ///
    /// That disclosure is `chat.rs::announce_model_choice`, on the `zirv
    /// \u{25b8}` announcement channel, **not** the launch banner. The banner
    /// alone was not enough to carry the exemption: `chrome.banner` is not
    /// `REPO_FORBIDDEN`, so the same repo layer that set this key could set
    /// `[chrome] banner = false` beside it and choose the model with nothing
    /// shown anywhere (the `wrap` fallback has no other model surface at
    /// all). `chrome.events` **is** `REPO_FORBIDDEN`, so the announcement is
    /// one a repo cannot silence -- only the operator can, with
    /// `--quiet`/`ZIRV_CTX_QUIET`. The banner and the dashboard header still
    /// show it too, as the standing on-screen copy.
    pub model: Option<String>,

    /// Issue #504: overrides the INTERACTIVE launch's Claude Code
    /// `--permission-mode`, one of `"default"` (the shipped posture: every
    /// action outside the projected allow-list prompts), `"acceptEdits"` or
    /// `"bypassPermissions"`. `None` (the default) reproduces `"default"`
    /// exactly, so behavior is unchanged unless an operator sets this.
    /// Headless launches are untouched either way -- they always carry
    /// `dontAsk` -- and this key never suppresses or widens the
    /// `--allowedTools`/`--disallowedTools` lists themselves, even under
    /// `bypassPermissions`: only the mode flag changes.
    ///
    /// Reached, before this key existed, only by editing the operator's own
    /// `~/.claude/settings.json` `permissions.defaultMode` -- which the CLI
    /// flag zirv always passes silently outranks, so that setting had no
    /// effect (the observed report, issue #504: an operator running several
    /// native subagents delegating into worktrees outside the launch cwd's
    /// own `./**` scope got prompted for every Edit/Write and every
    /// unlisted compound command, with no config knob to quiet it).
    ///
    /// `REPO_FORBIDDEN`: a repository checkout must not be able to widen its
    /// own session's permission posture -- the same trust asymmetry
    /// `sandbox.enabled`/`sandbox.extra_allow` already hold, applied to this
    /// adapter-native flag instead. Set it in `~/.zirv/ctx.toml`, or with
    /// `ZIRV_CTX_CHAT_CLAUDE_PERMISSION_MODE`. Validated at load (see
    /// `CtxConfig::load`) against the same fixed set Claude Code's own CLI
    /// accepts; an unrecognized value is a load-time error naming the key,
    /// not a silent fallback to `"default"`.
    pub claude_permission_mode: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_defaults_inject_with_a_capped_repo_layer() {
        let prompt = PromptConfig::default();
        assert!(prompt.enabled);
        assert!(prompt.repo_layer);
        assert_eq!(prompt.max_repo_bytes, 4096);
        assert!(prompt.harnesses);
        assert!(prompt.codex_orchestrator);
    }

    #[test]
    fn context_defaults_match_prompt_max_repo_bytes() {
        let context = ContextConfig::default();
        assert_eq!(context.max_common_bytes, 4096);
        assert_eq!(context.max_harness_bytes, 4096);
        assert_eq!(context.max_harness_roster_bytes, 4096);
    }

    #[test]
    fn mail_defaults_are_enabled_with_sane_caps() {
        let mail = MailConfig::default();
        assert!(mail.enabled, "the mailbox is on by default");
        assert_eq!(mail.max_message_bytes, 4096);
        assert_eq!(mail.max_delivered_bytes, 4096);
        assert_eq!(mail.keep, 50);
    }

    #[test]
    fn chrome_defaults_are_all_on() {
        let chrome = ChromeConfig::default();
        assert!(chrome.banner, "the launch banner is on by default");
        assert!(chrome.bar, "the status bar is on by default");
        assert!(chrome.events, "the announcement channel is on by default");
    }

    #[test]
    fn memory_defaults_are_enabled_off_harvest_with_sane_caps() {
        let memory = MemoryConfig::default();
        assert!(memory.enabled, "the private memory bank is on by default");
        assert!(
            !memory.harvest,
            "automatic harvesting is off by default: remembering is a deliberate act"
        );
        assert_eq!(memory.max_entries, 50);
        assert_eq!(memory.max_entry_bytes, 512);
        assert_eq!(memory.max_injected_bytes, 2048);
        assert!(
            memory.shared_enabled,
            "the shared (repo-owned) scope is on by default too"
        );
        assert_eq!(memory.core_max_bytes, 2048);
        assert_eq!(memory.retrieval_max_bytes, 2048);
        assert_eq!(memory.retrieval_max_entries, 6);
        assert_eq!(
            memory.harvest_max_entries, 5,
            "one session's own harvest stays conservative by default"
        );
        assert_eq!(memory.harvest_max_bytes, 2048);
    }

    /// Issue #326 B1: default budget for `task::compile_task_prompt`'s own
    /// `## PARENT OUTCOMES` block.
    #[test]
    fn task_config_defaults_to_a_4096_byte_parent_outcome_budget() {
        assert_eq!(TaskConfig::default().max_parent_outcome_bytes, 4096);
    }

    #[test]
    fn dash_defaults_are_on_with_a_28_col_sidebar() {
        let cfg = CtxConfig::default();
        assert!(cfg.dash.enabled);
        assert_eq!(cfg.dash.sidebar_cols, 28);
        assert_eq!(cfg.dash.roster_max_age_secs, 604_800);
        assert_eq!(
            cfg.dash.max_panes, 9,
            "the default cap matches Ctrl+A 1..9 addressing"
        );
        assert!(
            cfg.dash.mouse,
            "the wheel scrolls a pane's scrollback out of the box"
        );
        assert_eq!(cfg.dash.idle_quiet_ms, 10_000);
        assert_eq!(cfg.dash.motion, DashMotion::Full);
    }

    #[test]
    fn chat_model_defaults_to_none() {
        assert_eq!(ChatConfig::default().model, None);
    }
}
