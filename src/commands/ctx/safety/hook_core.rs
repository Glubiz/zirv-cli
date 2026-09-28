//! Hook rules for command safety.

use super::*;

/// Claude PreToolUse payload fields used by this hook. Missing or malformed
/// optional fields fail open; only explicit interactive permission modes
/// prove a human can answer Ask, so unknown modes are headless.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct HookToolPayload {
    session_id: String,
    /// Present only inside a native Claude subagent. Such a call is already
    /// the delegated worker the orchestrator-write rule asks the seat to use.
    agent_id: String,
    tool_name: String,
    tool_input: HookToolInput,
    permission_mode: String,
    /// Working directory for resolving relative repo writes; falls back to
    /// the hook process directory when absent (#334).
    cwd: String,
    /// Transcript path for the identical-command guard; absent or invalid
    /// values disable that best-effort signal (#313).
    #[serde(default)]
    transcript_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct HookToolInput {
    command: String,
    #[serde(rename = "dangerouslyDisableSandbox")]
    dangerously_disable_sandbox: bool,
}

impl HookToolPayload {
    fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

/// Claude PreToolUse decision envelope. Interactive Allow suppresses the
/// native prompt; under `dontAsk`, Allow and Ask normally emit nothing so
/// native permissions retain control. Deny always emits `deny`. An Ask in
/// `dontAsk` cannot be answered and would override operator allow rules
/// (#102). Additional context can require an explicit Allow envelope (#313).
#[cfg(test)]
pub(super) fn hook_output(
    command: &str,
    outcome: &Outcome,
    permission_mode: &str,
    divergence: SnapshotDivergence,
    status: &str,
) -> Option<String> {
    hook_output_with_extras(
        command,
        outcome,
        permission_mode,
        hook_launch_mode(permission_mode, &|_| None),
        divergence,
        status,
        &HookOutputExtras::default(),
    )
}

/// Keep denial and identical-command breaker text in separate verdict
/// branches so one cannot overwrite the other's reason (#313).
#[derive(Debug, Clone, Default)]
struct HookOutputExtras {
    /// Stop a denial loop while retaining the policy reason the operator
    /// needs to understand the block (#313).
    breaker_note: Option<String>,
    /// Name the repeated-failure refusal instead of an ordinary policy
    /// reason that would misstate why headless execution stopped (#313).
    reason_override: Option<String>,
    /// Emit a warning even when `dontAsk` would otherwise suppress Allow
    /// output, so the repeated-failure signal reaches the agent (#313).
    additional_context: Option<String>,
}

/// Build the hook reason from policy text and breaker overrides, appending
/// a blocked-action instruction only for a visible block.
/// block, appended at the very end.
fn hook_reason_text(
    command: &str,
    outcome: &Outcome,
    mode: super::adapters::LaunchMode,
    divergence: SnapshotDivergence,
    status: &str,
    extras: &HookOutputExtras,
) -> String {
    let base = extras
        .reason_override
        .clone()
        .unwrap_or_else(|| explain_text(command, outcome, mode, divergence, status, None));
    let text = match &extras.breaker_note {
        Some(note) => format!("{note} -- {base}"),
        None => base,
    };
    match blocked_instruction_suffix(command, outcome.verdict) {
        Some(suffix) => format!("{text} {suffix}"),
        None => text,
    }
}

/// Add instructions only for visible Ask/Deny decisions. Use the command
/// family here because the full command is already shown in this reason;
/// persisted logs use a stricter family to avoid exposing arguments.
fn blocked_instruction_suffix(command: &str, verdict: Verdict) -> Option<String> {
    match verdict {
        Verdict::Deny | Verdict::Ask => {
            let family = super::hook::command_family(command);
            let family = if family.is_empty() {
                "unknown".to_string()
            } else {
                family
            };
            Some(format!(
                "BLOCKED: {family}. Do not retry this command or work around it -- report \
                 `BLOCKED: {family}` to your caller and move on to other work."
            ))
        }
        Verdict::Allow => None,
    }
}

/// Builds the JSON envelope [`hook_output_with_extras`] emits, with
/// `additional_context` folded into `hookSpecificOutput.additionalContext`
/// when present -- the one place both this new field and the pre-existing
/// three ever get serialized, so the shape can never drift between the
/// `Some`/`None` cases.
fn hook_output_json(decision: &str, reason: String, additional_context: Option<&str>) -> String {
    let mut value = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    });
    if let Some(ctx) = additional_context {
        value["hookSpecificOutput"]["additionalContext"] =
            serde_json::Value::String(ctx.to_string());
    }
    value.to_string()
}

/// Render a hook decision with breaker context; default extras preserve
/// the ordinary decision envelope (#313).
fn hook_output_with_extras(
    command: &str,
    outcome: &Outcome,
    permission_mode: &str,
    mode: super::adapters::LaunchMode,
    divergence: SnapshotDivergence,
    status: &str,
    extras: &HookOutputExtras,
) -> Option<String> {
    let dont_ask = permission_mode == "dontAsk";
    let decision = match outcome.verdict {
        Verdict::Deny => "deny",
        // Under `dontAsk`, Ask cannot prompt and can override operator native
        // allow rules; protect config edits with an explicit approval gate (#102).
        Verdict::Ask if dont_ask && !operator_config_approval(outcome) => return None,
        Verdict::Ask => "ask",
        // Suppress ordinary decisions under `dontAsk` to preserve native rules.
        // Emit identical-command warnings so the agent can break a retry loop (#313).
        Verdict::Allow if dont_ask => {
            return extras.additional_context.as_deref().map(|ctx| {
                hook_output_json(
                    "allow",
                    hook_reason_text(command, outcome, mode, divergence, status, extras),
                    Some(ctx),
                )
            });
        }
        // Interactive Allow must be explicit: native defaults would prompt for
        // commands this policy already cleared.
        Verdict::Allow => "allow",
    };
    Some(hook_output_json(
        decision,
        hook_reason_text(command, outcome, mode, divergence, status, extras),
        extras.additional_context.as_deref(),
    ))
}

/// Check a command or a Claude PreToolUse payload. CLI mode returns a
/// verdict exit code; hook mode fails open on unreadable or irrelevant input
/// and expresses decisions in stdout with exit zero. Suppress a stale
/// standalone registration when the consolidated Bash hook is installed,
/// avoiding two evaluations of one tool call (#769).
pub fn run_check<W: Write>(args: &CheckArgs, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let cfg = CtxConfig::load(&args.repo, env)?;

    if !args.command.is_empty() {
        let command = args.command.join(" ");
        let scratchpad_roots = scratchpad_write_roots(&std::env::temp_dir());
        let cwd = std::env::current_dir().ok();
        let cwd = cwd.as_deref();
        let now = super::state::now_secs();
        let envelope = parse_envelope_env(env);
        let outcome = evaluate_with_scratchpad_roots(
            &cfg.safety,
            &command,
            args.mode,
            &scratchpad_roots,
            envelope.as_ref(),
            cwd,
            now,
        );
        writeln!(w, "{}", render_outcome(&command, &outcome))?;
        return Ok(outcome.verdict.exit_code());
    }

    if crate::commands::setup::claude_pretool_hook_runs_bash_safety_itself() {
        return Ok(0);
    }
    run_check_hook_mode_for_agent(&cfg, w, &read_stdin(), env, args.agent.as_deref())
}

/// Project non-Claude payloads through the shared safety check so adapters
/// cannot drift on verdict semantics; the consolidated hook calls it
/// in-process for Bash/PowerShell (#418, #769).
pub(crate) fn run_check_hook_mode_for_agent<W: Write>(
    cfg: &CtxConfig,
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
    agent: Option<&str>,
) -> CtxResult<i32> {
    match agent {
        None | Some("claude") => run_check_hook_mode_with_env(cfg, w, stdin, env),
        Some(name) => {
            let Some(projected) = super::hook_project::project_pretool(name, stdin) else {
                return Ok(0);
            };
            let mut buf: Vec<u8> = Vec::new();
            let code = run_check_hook_mode_with_env(cfg, &mut buf, &projected, env)?;
            let claude_envelope = String::from_utf8(buf)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            if let Some(translated) =
                super::hook_project::translate_pretool_envelope(name, claude_envelope)
            {
                writeln!(w, "{translated}")?;
            }
            Ok(code)
        }
    }
}

/// Verify the exact interactive-launch pin inherited by the hook process;
/// absence or any other value cannot prove an operator is present.
fn launch_mode_pinned_interactive(env: EnvLookup<'_>) -> bool {
    env(super::adapters::LAUNCH_MODE_ENV).as_deref()
        == Some(super::adapters::LAUNCH_MODE_INTERACTIVE_VALUE)
}

/// A permission mode comes from a human-attended launch or zirv's headless
/// launchers, which always pass `dontAsk`. Every other non-empty mode is
/// interactive, including `auto`, `bypassPermissions`, and future modes.
/// A missing mode fails closed to headless; zirv's interactive launch pin
/// takes precedence over the payload in every case.
fn hook_launch_mode(permission_mode: &str, env: EnvLookup<'_>) -> super::adapters::LaunchMode {
    if launch_mode_pinned_interactive(env) || !matches!(permission_mode, "" | "dontAsk") {
        super::adapters::LaunchMode::Interactive
    } else {
        super::adapters::LaunchMode::Headless
    }
}

/// Every scratchpad root the confined-write classifier accepts; shared with
/// the launch-settings projection via `adapters::scratchpad_roots`, forward-
/// slash normalized with no trailing separator.
pub(super) fn scratchpad_write_roots(temp_dir: &std::path::Path) -> Vec<String> {
    super::adapters::scratchpad_roots(temp_dir)
}

/// The primary scratchpad root (tests build confined targets under it).
#[cfg(test)]
pub(super) fn scratchpad_write_root(temp_dir: &std::path::Path) -> String {
    scratchpad_write_roots(temp_dir).remove(0)
}

/// Strip a leading literal `cd` only for known roots and a following
/// command. Dynamic paths and `..` remain unproven and unstripped (#168).
pub(crate) fn strip_known_root_cd_prefix(
    command: &str,
    allowed_roots: &[String],
) -> Option<String> {
    let (normalized, remainder) = parse_leading_cd_segment(command)?;
    let under_worktrees =
        normalized.contains(".claude/worktrees/") || normalized.ends_with(".claude/worktrees");
    let under_allowed_root = allowed_roots.iter().any(|root| {
        !root.is_empty() && (normalized == *root || normalized.starts_with(&format!("{root}/")))
    });
    if under_worktrees || under_allowed_root {
        Some(remainder)
    } else {
        None
    }
}

/// The roots [`strip_known_root_cd_prefix`] treats as known-safe `cd`
/// targets: the session scratchpad roots, and this process's own working
/// directory (the repo root, for a hook process launched from inside it).
fn cd_allow_roots(scratchpad_roots: &[String]) -> Vec<String> {
    let mut roots = scratchpad_roots.to_vec();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(
            cwd.to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_string(),
        );
    }
    roots
}

/// Resolve the hook payload cwd, falling back to process cwd; if neither
/// exists, no repo-write target can be proven (#334).
fn hook_repo_root(cwd: &str) -> Option<String> {
    if !cwd.is_empty() {
        return Some(cwd.replace('\\', "/").trim_end_matches('/').to_string());
    }
    std::env::current_dir().ok().map(|path| {
        path.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string()
    })
}

/// The program-family label for `OrchestratorBlock.target` on a Bash/
/// PowerShell repository-write refusal (issues #328/#334): `command`'s
/// first segment's first token, plus -- for `sed`/`perl` -- whichever
/// in-place flag is present, e.g. `"sed -i"`. Never the full command text:
/// `OrchestratorBlock` is privacy-preserving like `SafetyDecision`.
fn orchestrator_block_tool_family(command: &str) -> String {
    let sanitized = redact_single_quoted_heredocs(command);
    let Some(first_segment) = split_segments(&sanitized).into_iter().next() else {
        return String::new();
    };
    let collapsed = collapse_whitespace(&first_segment);
    let Some(tokens) = sql_tokens(&collapsed) else {
        return String::new();
    };
    let Some(first) = tokens.first() else {
        return String::new();
    };
    let program = sql_program_name(first);
    if matches!(program.as_str(), "sed" | "perl")
        && let Some(flag) = tokens[1..].iter().find(|t| is_sed_perl_inplace_flag(t))
    {
        return format!("{program} {flag}");
    }
    program
}

/// Count a trailing run of failed Bash calls with exactly matching command
/// text in a session JSONL transcript. Pair tool use and result by ID,
/// ignoring malformed lines; successful calls reset the count (#313).
/// Read only the transcript tail to bound PreToolUse latency.
const GUARD_TRANSCRIPT_TAIL_BYTES: u64 = 2 * 1024 * 1024;

fn read_transcript_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > GUARD_TRANSCRIPT_TAIL_BYTES {
        file.seek(SeekFrom::Start(len - GUARD_TRANSCRIPT_TAIL_BYTES))
            .ok()?;
    }
    let mut bytes = Vec::with_capacity(len.min(GUARD_TRANSCRIPT_TAIL_BYTES) as usize);
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn trailing_same_command_failure_run(jsonl: &str, command: &str) -> usize {
    use std::collections::HashMap;

    let mut pending_bash: HashMap<String, String> = HashMap::new();
    let mut run = 0usize;

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let message = row
            .get("message")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match row.get("type").and_then(serde_json::Value::as_str) {
            Some("user") => {
                let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array)
                else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_result")
                    {
                        continue;
                    }
                    let Some(matched_command) = block
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| pending_bash.remove(id))
                    else {
                        continue;
                    };
                    if matched_command != command {
                        continue;
                    }
                    let is_error =
                        block.get("is_error").and_then(serde_json::Value::as_bool) == Some(true);
                    run = if is_error { run + 1 } else { 0 };
                }
            }
            Some("assistant") => {
                let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array)
                else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let tool_name = block
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if !tool_name.eq_ignore_ascii_case("Bash") {
                        continue;
                    }
                    if let (Some(id), Some(cmd)) = (
                        block.get("id").and_then(serde_json::Value::as_str),
                        block
                            .get("input")
                            .and_then(|input| input.get("command"))
                            .and_then(serde_json::Value::as_str),
                    ) {
                        pending_bash.insert(id.to_string(), cmd.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    run
}

/// Evaluate hook stdin lazily so CLI mode never blocks waiting for it.
pub(super) fn run_check_hook_mode_with_env<W: Write>(
    cfg: &CtxConfig,
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    run_check_hook_with_verdict(cfg, w, stdin, env).map(|_| 0)
}

/// Return the verdict even when headless output is silent so rehydration can
/// screen the actual command (#466).
pub(crate) fn run_check_hook_with_verdict<W: Write>(
    cfg: &CtxConfig,
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
) -> CtxResult<Option<Verdict>> {
    let Some(payload) = HookToolPayload::parse(stdin) else {
        return Ok(None);
    };
    if !matches!(payload.tool_name.as_str(), "Bash" | "PowerShell") {
        return Ok(None);
    }
    let command = payload.tool_input.command.trim();
    if command.is_empty() {
        return Ok(None);
    }
    let mode = hook_launch_mode(&payload.permission_mode, env);
    // Strip a proven `cd` prefix so it cannot force a fallback verdict;
    // retain the original text in decisions and audit output (#168).
    let scratchpad_roots = scratchpad_write_roots(&std::env::temp_dir());
    let cd_roots = cd_allow_roots(&scratchpad_roots);
    let effective_command =
        strip_known_root_cd_prefix(command, &cd_roots).unwrap_or_else(|| command.to_string());

    // Detect orchestrator repo writes before any widening. Hard Deny stays
    // final; Advise and Allow follow ordinary evaluation and carry audit data
    // until the final verdict is known (#328, #334, #358).
    let orchestrator_repo_write = (env(super::adapters::SEAT_ROLE_ENV).as_deref()
        == Some("orchestrator")
        && payload.agent_id.is_empty())
    .then(|| hook_repo_root(&payload.cwd))
    .flatten()
    .and_then(|cwd| {
        orchestrator_repo_write_target(&effective_command, &cwd, &filesystem_repo_root_of, env)
    });
    let orchestrator_posture = super::lifecycle::orchestrator_write_posture(cfg);

    let cwd = if payload.cwd.is_empty() {
        std::env::current_dir().ok()
    } else {
        Some(Path::new(&payload.cwd).to_path_buf())
    };
    let cwd = cwd.as_deref();
    let evidence = evaluate_with_attestation_evidence(
        &cfg.safety,
        &effective_command,
        mode,
        env,
        &scratchpad_roots,
        cwd,
    );
    let mut outcome = evidence.outcome.clone();
    let mut orchestrator_advisory: Option<String> = None;
    // Defer the repo-write audit row until the verdict is final: later guards
    // can still turn an allowed write into Deny (#358).
    let mut orchestrator_block_pending: Option<(String, String)> = None;
    if let Some(target) = orchestrator_repo_write {
        let session =
            super::mail::session_identity(env).unwrap_or_else(|| payload.session_id.clone());
        match orchestrator_posture {
            super::config::OrchestratorWrites::Deny => {
                outcome = Outcome {
                    verdict: Verdict::Deny,
                    matched: Some(Rule {
                        pattern: format!(
                            "<orchestrator seat: dispatch a worker -- repository write to {target}>"
                        ),
                        origin: Origin::BuiltIn,
                    }),
                };
            }
            super::config::OrchestratorWrites::Advise => {
                if super::hook::orchestrator_advisory_should_surface(env, &session) {
                    orchestrator_advisory = Some(format!(
                        "orchestrator seat wrote to {target}: fine for a trivial edit; \
                         delegate substantial changes to a worker"
                    ));
                }
            }
            super::config::OrchestratorWrites::Allow => {}
        }
        orchestrator_block_pending = Some((session, target));
    }
    // Apply scratchpad confinement before sandbox retry so confined writes
    // remain allowed on both paths (#168).
    if outcome.verdict != Verdict::Deny
        && every_segment_is_allow_or_unmatched_default(
            &cfg.safety,
            &effective_command,
            cfg.safety.default_verdict(mode),
            &scratchpad_roots,
        )
        && write_targets_confined(&effective_command, &scratchpad_roots) == Some(true)
        && split_segments(&effective_command)
            .iter()
            .all(|candidate| !escape_denied_by_screen_with_redirects(candidate, true))
    {
        outcome = Outcome {
            verdict: Verdict::Allow,
            matched: Some(Rule {
                pattern: "<scratchpad: confined write>".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
    }
    // Preserve semantic Deny before considering unsandboxed retries. Read-only
    // forge calls and operator-cleared segments still pass credential/root
    // screens; unsafe or merely asked commands retain escalation (#147).
    if payload.tool_input.dangerously_disable_sandbox
        && outcome.verdict != Verdict::Deny
        && !operator_config_approval(&outcome)
    {
        let already_scratchpad_confined = outcome
            .matched
            .as_ref()
            .is_some_and(|rule| rule.pattern == "<scratchpad: confined write>");
        outcome = if already_scratchpad_confined {
            outcome
        } else if outcome.verdict == Verdict::Allow
            && is_sandbox_bypass_safe_gh_command(&effective_command)
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: read-only gh>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && is_reserved_zirv_escape_safe(&effective_command)
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: reserved zirv built-in>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && is_read_only_escape_safe(&effective_command, &scratchpad_roots)
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: read-only escape>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if is_mixed_confined_write_and_read_only_escape_safe(
            &cfg.safety,
            &effective_command,
            cfg.safety.default_verdict(mode),
            &scratchpad_roots,
        ) {
            // Mixed confined writes and safe reads can fold to whole-command Ask;
            // check each segment's policy and require a real confined write (#321).
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: confined write + read-only escape>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if outcome.verdict == Verdict::Allow
            && escape_allow_matches(
                &cfg.safety.escape_allow,
                &effective_command,
                &scratchpad_roots,
                cwd,
            )
        {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: escape_allow>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else if retry_has_allow_verdict(
            &cfg.safety,
            &effective_command,
            &outcome,
            &scratchpad_roots,
        ) && allow_verdict_retry_clears_escape_screen(
            &effective_command,
            &scratchpad_roots,
            cwd,
        ) {
            Outcome {
                verdict: Verdict::Allow,
                matched: Some(Rule {
                    pattern: "<sandbox: allow-verdict retry>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        } else {
            Outcome {
                verdict: if mode.is_interactive() {
                    Verdict::Ask
                } else {
                    Verdict::Deny
                },
                matched: Some(Rule {
                    pattern: "<sandbox: unsandboxed retry>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            }
        };
    }
    // Count prior denials before logging this one, then include the current
    // verdict with `+ 1` when deciding whether to stop retries (#313).
    let mut breaker_note: Option<String> = None;
    if matches!(outcome.verdict, Verdict::Ask | Verdict::Deny)
        && !payload.session_id.is_empty()
        && cfg.safety.denial_breaker_threshold > 0
        && let Ok(state) = super::state::StateDir::resolve(env)
    {
        let recent = super::log::read_recent_safety_decisions(
            &state,
            &payload.session_id,
            50,
            super::state::now_secs() / 86_400,
        );
        let trailing_denials = recent
            .iter()
            .rev()
            .take_while(|record| matches!(record.verdict.as_str(), "ask" | "deny"))
            .count() as u32;
        let total = trailing_denials + 1;
        if total >= cfg.safety.denial_breaker_threshold {
            breaker_note = Some(format!(
                "{total} consecutive denials this session: stop attempting variations of \
                 this command; report the blocker in your final message or via `zirv ctx \
                 send`, then continue with other work."
            ));
        }
    }

    // Guard only otherwise allowed repeated failures; already blocked and
    // read-only inspection commands need no additional refusal (#313).
    let mut reason_override: Option<String> = None;
    // Retain both the repo-write advisory and repeated-failure warning when
    // both apply to an allowed command (#358).
    let mut additional_context: Option<String> = orchestrator_advisory;
    if outcome.verdict == Verdict::Allow
        && cfg.safety.identical_command_warn_after > 0
        && !is_read_only_escape_safe(command, &scratchpad_roots)
        && let Some(transcript_path) = payload.transcript_path.as_deref()
        && let Some(transcript) = read_transcript_tail(Path::new(transcript_path))
    {
        let run = trailing_same_command_failure_run(&transcript, command);
        let refuses = cfg.safety.identical_command_refuse_after > 0
            && run >= cfg.safety.identical_command_refuse_after as usize
            && mode == super::adapters::LaunchMode::Headless;
        if refuses {
            outcome = Outcome {
                verdict: Verdict::Deny,
                matched: Some(Rule {
                    pattern: "<guard: identical failing command>".to_string(),
                    origin: Origin::BuiltIn,
                }),
            };
            reason_override = Some(format!(
                "zirv guard: this exact command has failed {run} times in a row; refusing \
                 to run it again -- report the blocker."
            ));
        } else if run >= cfg.safety.identical_command_warn_after as usize {
            let identical_command_note = format!(
                "zirv guard: this exact command has failed {run} times in a row; change the \
                 approach or the input before running it again."
            );
            additional_context = Some(match additional_context {
                Some(existing) => format!("{existing} | {identical_command_note}"),
                None => identical_command_note,
            });
        }
    }

    if outcome.verdict == Verdict::Allow
        && evidence
            .outcome
            .matched
            .as_ref()
            .is_some_and(|rule| rule.pattern == "<filesystem: generated-directory cleanup>")
        && let Some(cwd) = cwd
        && let Some(tokens) = sql_tokens(&effective_command)
        && tokens
            .iter()
            .skip(1)
            .filter(|target| generated_path(target))
            .any(|target| {
                Path::new(target)
                    .ancestors()
                    .filter(|path| !path.as_os_str().is_empty())
                    .any(|path| {
                        std::fs::symlink_metadata(cwd.join(path))
                            .is_ok_and(|meta| meta.file_type().is_symlink())
                    })
            })
    {
        outcome = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "<filesystem: generated-directory symlink>".into(),
                origin: Origin::BuiltIn,
            }),
        };
    }

    // Run Jev after deterministic adjustments so it sees the final local
    // verdict and any escalation clears stale Allow context. Skip `dontAsk`,
    // where its answer cannot change the decision (#781).
    if payload.permission_mode != "dontAsk"
        && let Ok(state) = super::state::StateDir::resolve(env)
    {
        outcome = apply_jev_approve_outcome(
            cfg,
            &state,
            mode,
            &effective_command,
            &scratchpad_roots,
            outcome,
        );
    }

    // Clear Allow-only notes after a later Deny so advice cannot contradict
    // the actual refusal.
    if outcome.verdict != Verdict::Allow {
        additional_context = None;
    }

    // Audit the final repo-write disposition; a later guard may have changed
    // an advised or allowed write into Deny (#358).
    if let Some((session, _target)) = orchestrator_block_pending {
        let outcome_label = if outcome.verdict == Verdict::Deny {
            "denied"
        } else {
            match orchestrator_posture {
                super::config::OrchestratorWrites::Advise => "advised",
                _ => "allowed",
            }
        };
        if let Ok(state) = super::state::StateDir::resolve(env) {
            let tool_family = orchestrator_block_tool_family(&effective_command);
            let _ = super::log::append_orchestrator_block(
                &state,
                &super::log::OrchestratorBlock {
                    ts: super::state::now_secs(),
                    session: &session,
                    tool: &payload.tool_name,
                    target: &tool_family,
                    reason: "repository write",
                    outcome: outcome_label,
                },
            );
        }
    }

    let extras = HookOutputExtras {
        breaker_note,
        reason_override,
        additional_context,
    };
    if let Some(output) = hook_output_with_extras(
        command,
        &outcome,
        &payload.permission_mode,
        mode,
        evidence.divergence,
        evidence.status,
        &extras,
    ) {
        writeln!(w, "{output}")?;
    }
    audit_hook_decision(&payload, command, mode, &outcome, &evidence, env);
    Ok(Some(outcome.verdict))
}

/// Dispatcher programs whose known subcommands may appear in persisted
/// safety-family logs. Other programs expose only their name, never raw
/// arguments that may contain secrets or paths.
const SAFETY_FAMILY_DISPATCHER_PROGRAMS: &[&str] = &[
    "git",
    "gh",
    "glab",
    "docker",
    "kubectl",
    "cargo",
    "npm",
    "npx",
    "gitlab-ci-local",
    "zirv",
    "codex",
    "terraform",
    "aws",
];

/// The safety-decision log's own family derivation: `hook::command_family`'s
/// candidate, narrowed to its first word alone unless `argv[0]` is one of
/// `SAFETY_FAMILY_DISPATCHER_PROGRAMS` -- see that constant's own doc
/// comment for why a plain narrower rule is not safe enough here. `pub(crate)`
/// so a test elsewhere (`agent::tests::blocked_family_lines_...`) can build a
/// realistic fixture record without duplicating this rule.
pub(crate) fn safety_family(command: &str) -> String {
    let full = super::hook::command_family(command);
    let program = full.split(' ').next().unwrap_or("");
    if program.is_empty() || !SAFETY_FAMILY_DISPATCHER_PROGRAMS.contains(&program) {
        return program.to_string();
    }
    full
}

fn audit_hook_decision(
    payload: &HookToolPayload,
    command: &str,
    mode: super::adapters::LaunchMode,
    outcome: &Outcome,
    evidence: &AttestedEvaluation,
    env: EnvLookup<'_>,
) {
    if payload.session_id.is_empty() {
        return;
    }
    let Ok(state) = super::state::StateDir::resolve(env) else {
        return;
    };
    let command_fingerprint = sha256_hex(command.as_bytes());
    let matched_pattern = outcome.matched.as_ref().map(|rule| rule.pattern.as_str());
    let origin = outcome.matched.as_ref().map(|rule| rule.origin.label());
    // Change 5a: plaintext by design, unlike `command_fingerprint` above.
    let family = safety_family(command);
    let decision = super::log::SafetyDecision {
        ts: super::state::now_secs(),
        session: &payload.session_id,
        mode: mode.label(),
        verdict: outcome.verdict.label(),
        family: &family,
        command_sha256: &command_fingerprint,
        policy_sha256: &evidence.current_fingerprint,
        launch_policy_sha256: evidence.launch_fingerprint.as_deref(),
        attestation: evidence.status,
        matched_pattern,
        origin,
        platform: std::env::consts::OS,
    };
    let _ = super::log::append_safety(&state, &decision);
}

#[cfg(test)]
pub(super) fn run_check_hook_mode<W: Write>(
    cfg: &CtxConfig,
    w: &mut W,
    stdin: &str,
) -> CtxResult<i32> {
    run_check_hook_mode_with_env(cfg, w, stdin, &|_| None)
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    // -- Issue #418: non-claude agent projection ---------------------------

    /// A droid `Execute` payload naming a command the safety policy denies
    /// outright yields the claude-identical `hookSpecificOutput` deny
    /// envelope; the gemini variant of the same command yields gemini's own
    /// `{"decision":"deny","reason":...}` shape. Drives `run_check_hook_
    /// mode_for_agent` three ways (claude, droid, gemini) against the SAME
    /// underlying command and compares against the actual claude output,
    /// rather than a hand-written expectation, so a change to the deny
    /// reason text cannot silently desync this test from production.
    #[test]
    fn droid_and_gemini_projected_denies_match_claude() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let env = |_: &str| None;

        let claude_stdin = r#"{"tool_name":"Bash","tool_input":{"command":"sudo rm -rf /"}}"#;
        let mut claude_out = Vec::new();
        run_check_hook_mode_for_agent(&cfg, &mut claude_out, claude_stdin, &env, None)
            .expect("runs");
        let claude_text = String::from_utf8(claude_out).expect("utf8");
        let claude_json: serde_json::Value =
            serde_json::from_str(claude_text.trim()).expect("json");
        assert_eq!(
            claude_json["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        let reason = claude_json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("a reason")
            .to_string();

        let droid_stdin = r#"{"session_id":"s1","cwd":"/repo","tool_name":"Execute","tool_input":{"command":"sudo rm -rf /"}}"#;
        let mut droid_out = Vec::new();
        run_check_hook_mode_for_agent(&cfg, &mut droid_out, droid_stdin, &env, Some("droid"))
            .expect("runs");
        let droid_text = String::from_utf8(droid_out).expect("utf8");
        assert_eq!(
            droid_text.trim(),
            claude_text.trim(),
            "droid documents claude's exact hookSpecificOutput shape"
        );

        let gemini_stdin = r#"{"session_id":"s1","cwd":"/repo","tool_name":"run_shell_command","tool_input":{"command":"sudo rm -rf /"}}"#;
        let mut gemini_out = Vec::new();
        run_check_hook_mode_for_agent(&cfg, &mut gemini_out, gemini_stdin, &env, Some("gemini"))
            .expect("runs");
        let gemini_text = String::from_utf8(gemini_out).expect("utf8");
        let gemini_json: serde_json::Value =
            serde_json::from_str(gemini_text.trim()).expect("json");
        assert_eq!(gemini_json["decision"], "deny");
        assert_eq!(gemini_json["reason"], reason);
    }

    /// Review round (#418): a recognized field present with the wrong JSON
    /// type must fail the whole projection (empty stdout, exit 0), never be
    /// silently coerced into the "field absent" default and let a command
    /// the base policy would otherwise deny sail through unclassified.
    #[test]
    fn droid_projected_payload_with_a_wrongly_typed_field_fails_open() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let env = |_: &str| None;

        let droid_stdin =
            r#"{"tool_name":"Execute","cwd":7,"tool_input":{"command":"sudo rm -rf /"}}"#;
        let mut out = Vec::new();
        let code = run_check_hook_mode_for_agent(&cfg, &mut out, droid_stdin, &env, Some("droid"))
            .expect("runs");
        assert_eq!(code, 0);
        assert!(
            out.is_empty(),
            "a wrongly-typed recognized field must fail open: {out:?}"
        );
    }

    /// Review round (#418): a headless `ask` verdict on an unclassified
    /// command must not collapse to silence for a non-claude agent -- it
    /// would read as a silent allow, the wrong failure direction for a
    /// confirmation gate. Copilot/droid pass it through with the same
    /// reason claude gets; gemini has no `ask` concept and fails closed to a
    /// deny that still names the reason.
    #[test]
    fn projected_asks_do_not_collapse_to_silence() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let env = |_: &str| None;
        let command = "some-totally-unknown-tool --flag";

        let claude_stdin =
            format!(r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}}}}"#);
        let mut claude_out = Vec::new();
        run_check_hook_mode_for_agent(&cfg, &mut claude_out, &claude_stdin, &env, None)
            .expect("runs");
        let claude_text = String::from_utf8(claude_out).expect("utf8");
        let claude_json: serde_json::Value =
            serde_json::from_str(claude_text.trim()).expect("json");
        assert_eq!(
            claude_json["hookSpecificOutput"]["permissionDecision"], "ask",
            "sanity: an unclassified command must ask headlessly: {claude_text}"
        );
        let reason = claude_json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("a reason")
            .to_string();

        let copilot_stdin = format!(
            r#"{{"session_id":"s1","cwd":"/repo","tool_name":"bash","tool_input":{{"command":"{command}"}}}}"#
        );
        let mut copilot_out = Vec::new();
        run_check_hook_mode_for_agent(
            &cfg,
            &mut copilot_out,
            &copilot_stdin,
            &env,
            Some("copilot"),
        )
        .expect("runs");
        let copilot_text = String::from_utf8(copilot_out).expect("utf8");
        let copilot_json: serde_json::Value =
            serde_json::from_str(copilot_text.trim()).expect("json");
        assert_eq!(copilot_json["permissionDecision"], "ask");
        assert_eq!(copilot_json["permissionDecisionReason"], reason);

        let droid_stdin = format!(
            r#"{{"session_id":"s1","cwd":"/repo","tool_name":"Execute","tool_input":{{"command":"{command}"}}}}"#
        );
        let mut droid_out = Vec::new();
        run_check_hook_mode_for_agent(&cfg, &mut droid_out, &droid_stdin, &env, Some("droid"))
            .expect("runs");
        let droid_text = String::from_utf8(droid_out).expect("utf8");
        assert_eq!(
            droid_text.trim(),
            claude_text.trim(),
            "droid documents claude's exact envelope for ask too"
        );

        let gemini_stdin = format!(
            r#"{{"session_id":"s1","cwd":"/repo","tool_name":"run_shell_command","tool_input":{{"command":"{command}"}}}}"#
        );
        let mut gemini_out = Vec::new();
        run_check_hook_mode_for_agent(&cfg, &mut gemini_out, &gemini_stdin, &env, Some("gemini"))
            .expect("runs");
        let gemini_text = String::from_utf8(gemini_out).expect("utf8");
        let gemini_json: serde_json::Value =
            serde_json::from_str(gemini_text.trim()).expect("json");
        assert_eq!(
            gemini_json["decision"], "deny",
            "gemini has no ask concept, so it fails closed"
        );
        assert!(
            gemini_json["reason"]
                .as_str()
                .expect("a reason")
                .contains(&reason),
            "the original reason must survive: {gemini_json}"
        );
    }

    #[test]
    fn orchestrator_seat_allows_harness_memory_but_still_denies_repository_source() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let _vars = super::super::testenv::VarGuard::set(&[("CLAUDE_CONFIG_DIR", None)]);
        let harness_home = home.path().join(".claude");
        let repo = home.path().join("repo");
        for path in [&harness_home, &repo] {
            std::fs::create_dir_all(path).expect("repo dir");
            let status = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(path)
                .status()
                .expect("run git init");
            assert!(status.success(), "git init failed for {}", path.display());
        }

        // Issue #358 T8: pinned to `deny` explicitly -- this test's own
        // subject is the harness-home exemption, not posture, and the
        // default posture is now `advise`.
        let env = |key: &str| match key {
            super::super::adapters::SEAT_ROLE_ENV => Some("orchestrator".to_string()),
            "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES" => Some("deny".to_string()),
            "HOME" | "CLAUDE_CONFIG_DIR" => std::env::var(key).ok(),
            _ => None,
        };
        let cfg = CtxConfig::load(&repo, &env).expect("loads");
        let repo_cwd = repo.to_string_lossy().replace('\\', "/");

        for (command, denied) in [
            (
                format!(
                    "printf 'x' >> {}",
                    harness_home
                        .join("projects/slug/memory/MEMORY.md")
                        .display()
                ),
                false,
            ),
            ("printf 'x' >> src/x.rs".to_string(), true),
        ] {
            let stdin = serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": command},
                "permission_mode": "default",
                "cwd": repo_cwd,
            })
            .to_string();
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &env).expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert_eq!(
                text.contains("orchestrator seat: dispatch a worker"),
                denied,
                "{command}: got {text}"
            );
        }
    }

    /// End-to-end, per posture (issue #358 T8): an orchestrator seat's
    /// `sed -i` on a repository file denies through the hook under `deny`
    /// (naming the guard's own rule text), and proceeds under `advise`/
    /// `allow` -- `advise` carrying a rate-limited (first-write-always-
    /// surfaces) advisory note in `additionalContext`, `allow` carrying
    /// none.
    #[test]
    fn orchestrator_seat_repository_write_denies_through_the_hook() {
        for (posture, want_decision, want_advisory) in [
            ("deny", "deny", false),
            ("advise", "allow", true),
            ("allow", "allow", false),
        ] {
            let repo = tempfile::tempdir().expect("tempdir");
            // `filesystem_repo_root_of` only checks for a `.git` entry, so a
            // bare marker directory is enough to stand in for a real repo --
            // no `git init`/real git binary needed.
            std::fs::create_dir_all(repo.path().join(".git")).expect("fake git marker");
            let home = tempfile::tempdir().expect("tempdir");
            let _home = super::super::testenv::HomeGuard::set(home.path());
            let state_dir = tempfile::tempdir().expect("state dir");

            let mut env_map: HashMap<String, String> = HashMap::new();
            env_map.insert(
                super::super::adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            );
            env_map.insert(
                "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
                posture.to_string(),
            );
            env_map.insert(
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            );
            let cfg = CtxConfig::load(repo.path(), &|k| env_map.get(k).cloned()).expect("loads");
            let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");
            let command = "sed -i 's/a/b/' src/main.rs";
            let resolved = resolve_repo_write_target("src/main.rs", &repo_cwd)
                .expect("the relative repository target must resolve");
            assert!(
                filesystem_repo_root_of(&resolved).is_some(),
                "the resolved target {resolved} must retain the fake git ancestor at {repo_cwd}"
            );
            assert_eq!(
                orchestrator_repo_write_target(
                    command,
                    &repo_cwd,
                    &filesystem_repo_root_of,
                    &|k| env_map.get(k).cloned()
                ),
                Some("src/main.rs".to_string()),
                "the repository-write detector must remain independent of the command allow list"
            );

            let stdin = format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}},"permission_mode":"default","cwd":"{repo_cwd}"}}"#
            );
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
                .expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                text.contains(&format!(r#""permissionDecision":"{want_decision}""#)),
                "posture={posture}: got {text}"
            );
            assert_eq!(
                text.contains("orchestrator seat: dispatch a worker"),
                posture == "deny",
                "posture={posture}: got {text}"
            );
            assert_eq!(
                text.contains("fine for a trivial edit; delegate substantial changes to a worker"),
                want_advisory,
                "posture={posture}: got {text}"
            );
        }
    }

    #[test]
    fn orchestrator_seat_read_only_redirect_commands_allow_through_the_hook() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("fake git marker");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");
        let env_map = HashMap::from([(
            super::super::adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        )]);

        for command in [
            "zirv agent codex --workdir /x/wt338 - < /private/tmp/claude-501/session/scratchpad/brief-338.md",
            "for i in $(seq 1 80); do s=$(zirv ctx status --brief 2>&1); if echo \"$s\" | grep -qE 'x'; then echo READY; exit 0; fi; sleep 30; done",
        ] {
            let stdin = serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": command},
                "permission_mode": "default",
                "cwd": repo_cwd,
            })
            .to_string();
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
                .expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                text.contains(r#""permissionDecision":"allow""#),
                "the read-only command must allow from an orchestrator seat: {command}: got {text}"
            );
        }
    }

    /// The same repository-write command from a non-orchestrator seat (no
    /// role env at all, or an explicit `worker` role) is never denied by
    /// this rule -- the guard is scoped to the orchestrator seat only.
    #[test]
    fn orchestrator_seat_repository_write_guard_does_not_apply_off_the_orchestrator_seat() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"sed -i 's/a/b/' src/main.rs"}},"permission_mode":"default","cwd":"{repo_cwd}"}}"#
        );

        for env_map in [
            HashMap::new(),
            HashMap::from([(
                super::super::adapters::SEAT_ROLE_ENV.to_string(),
                "worker".to_string(),
            )]),
        ] {
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
                .expect("runs");
            let text = String::from_utf8(out).expect("utf8");
            assert!(
                !text.contains("orchestrator seat: dispatch a worker"),
                "{env_map:?}: got {text}"
            );
        }
    }

    #[test]
    fn orchestrator_seat_repository_write_guard_allows_a_native_subagent_bash_write() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("fake git marker");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");
        let stdin = serde_json::json!({
            "agent_id": "a1b2",
            "agent_type": "general-purpose",
            "tool_name": "Bash",
            "tool_input": {"command": "echo x > src/x.rs"},
            "permission_mode": "default",
            "cwd": repo_cwd,
        })
        .to_string();
        let env_map = HashMap::from([(
            super::super::adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        )]);

        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
            .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("orchestrator seat: dispatch a worker"),
            "a native subagent is the delegated worker: got {text}"
        );
        assert!(
            !text.contains(r#""permissionDecision":"deny""#),
            "the delegated worker's write must not be denied: got {text}"
        );
    }

    /// An orchestrator-seat write CONFINED to `.zirv/work` is not denied by
    /// this rule -- the scratchpad/allowed-root carve-out still applies.
    #[test]
    fn orchestrator_seat_write_confined_to_zirv_work_is_not_denied() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("fake git marker");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");

        let mut env_map: HashMap<String, String> = HashMap::new();
        env_map.insert(
            super::super::adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        );
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"echo x > .zirv/work/n.md"}},"permission_mode":"default","cwd":"{repo_cwd}"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
            .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            !text.contains("orchestrator seat: dispatch a worker"),
            "got {text}"
        );
    }

    /// A denied repository write appends one `orchestrator-blocks.jsonl`
    /// row whose `target` is a program-family label, never the full
    /// command text -- pinned to `deny` explicitly (issue #358 T8: the
    /// default posture is now `advise`).
    #[test]
    fn orchestrator_seat_repository_write_denial_appends_an_orchestrator_block() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("fake git marker");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let state_dir = tempfile::tempdir().expect("tempdir");

        let mut env_map: HashMap<String, String> = HashMap::new();
        env_map.insert(
            super::super::adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        );
        env_map.insert(
            super::super::state::STATE_ENV.to_string(),
            state_dir.path().to_string_lossy().to_string(),
        );
        env_map.insert(
            "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
            "deny".to_string(),
        );
        let cfg = CtxConfig::load(repo.path(), &|k| env_map.get(k).cloned()).expect("loads");
        let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");

        let command = "sed -i 's/a/b/' src/main.rs";
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}"}},"permission_mode":"default","cwd":"{repo_cwd}"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
            .expect("runs");

        let state = super::super::state::StateDir::from_root(state_dir.path().to_path_buf());
        let records = super::super::log::read_orchestrator_blocks(&state);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].tool, "Bash");
        assert_eq!(records[0].outcome, "denied");
        assert!(
            !records[0].target.contains(command),
            "target must not carry the full command text: {:?}",
            records[0].target
        );
    }

    /// Finding #9 (issue #358 review): the orchestrator-write audit row must
    /// reflect the FINAL verdict, not a label computed from `orchestrator_
    /// posture` alone before a LATER guard gets a chance to turn the
    /// `advise` posture's own `Allow` into a `Deny`. Here the later guard is
    /// the unsandboxed-retry escalation (`dangerouslyDisableSandbox`): under
    /// `advise` this repository write is not itself denied, but a headless
    /// retry outside the OS sandbox that clears none of that guard's own
    /// carve-outs still falls through to its `Deny` catch-all. The logged
    /// row must say "denied", never the stale "advised" the old ordering
    /// would have written before that guard ran.
    #[test]
    fn an_advised_write_later_denied_by_a_guard_logs_the_final_denied_outcome() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("fake git marker");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let state_dir = tempfile::tempdir().expect("tempdir");

        let mut env_map: HashMap<String, String> = HashMap::new();
        env_map.insert(
            super::super::adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        );
        env_map.insert(
            super::super::state::STATE_ENV.to_string(),
            state_dir.path().to_string_lossy().to_string(),
        );
        env_map.insert(
            "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
            "advise".to_string(),
        );
        let cfg = CtxConfig::load(repo.path(), &|k| env_map.get(k).cloned()).expect("loads");
        let repo_cwd = repo.path().to_string_lossy().replace('\\', "/");

        // 2026-09-16, spec Change 2: `sed -i` (this test's original example)
        // is now base-allowed everywhere, so it no longer demonstrates this
        // guard. `perl -pi` is the same in-place-edit shape
        // `orchestrator_repo_write_target` already recognizes (see
        // `orchestrator_repo_write_target_catches_repository_writes`), but
        // `perl` itself stays unmatched by any shipped rule, so the
        // scenario is intact: headless default `Ask`, unresolved by any
        // carve-out, falls through to the catch-all `Deny`.
        let command = "perl -pi -e 's/a/b/' src/main.rs";
        // `permission_mode: "dontAsk"` -> headless (see `run_check_hook_
        // mode_with_env`'s own mode derivation), which is what turns the
        // sandbox-bypass guard's catch-all into `Deny` rather than `Ask`.
        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"dontAsk","cwd":"{repo_cwd}"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|k| env_map.get(k).cloned())
            .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"deny""#),
            "sanity: the sandbox-bypass retry guard must be the one denying this: got {text}"
        );

        let state = super::super::state::StateDir::from_root(state_dir.path().to_path_buf());
        let records = super::super::log::read_orchestrator_blocks(&state);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            records[0].outcome, "denied",
            "the audit row must reflect the FINAL verdict, not the \"advised\" label computed \
             before the sandbox-bypass guard ran: {records:?}"
        );
    }

    #[test]
    fn the_hook_enforces_the_reviewed_envelope_table_and_allows_granted_commands() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let cfg = CtxConfig::load(repo.path(), &|_| None).unwrap();
        let mut envelope = safety_test_envelope();
        envelope.tools = envelope::ToolSet::none();
        envelope.network = false;
        envelope.expires_at = 1;
        let check = |envelope: &envelope::WorkerEnvelope, command, mode| {
            let raw = serde_json::to_string(envelope).unwrap();
            let env = |key: &str| (key == super::super::agent::ENVELOPE_ENV).then(|| raw.clone());
            let stdin = serde_json::json!({"tool_name":"Bash", "tool_input":{"command":command},
                "cwd":"/w", "permission_mode":mode, "session_id":"t"})
            .to_string();
            let mut out = Vec::new();
            assert_eq!(
                run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &env).unwrap(),
                0
            );
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()["hookSpecificOutput"]["permissionDecision"].as_str().unwrap().to_string()
        };
        for command in [
            "curl https://example.invalid",
            "echo allowed",
            "echo blocked > outside.txt",
            "cp input.txt outside.txt",
            "touch outside.txt",
            "echo blocked > /allowed/out.txt",
        ] {
            assert_eq!(check(&envelope, command, "dontAsk"), "deny", "{command}");
        }
        envelope.tools.shell = true;
        envelope.expires_at = u64::MAX;
        assert_eq!(check(&envelope, "echo allowed", "default"), "allow");
        envelope.network = true;
        envelope.tools.network = true;
        for command in ["echo x > allowed/out.txt", "cp a allowed/b"] {
            assert_eq!(check(&envelope, command, "default"), "allow", "{command}");
        }
        for command in [
            "cp a outside.txt",
            "touch outside.txt",
            "echo x > /allowed/out.txt",
        ] {
            assert_eq!(check(&envelope, command, "default"), "deny", "{command}");
        }
    }

    #[test]
    fn an_empty_hook_cwd_resolves_envelope_paths_against_the_process_directory() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let cfg = CtxConfig::load(repo.path(), &|_| None).unwrap();
        let envelope = safety_test_envelope();
        let raw = serde_json::to_string(&envelope).unwrap();
        let env = |key: &str| (key == super::super::agent::ENVELOPE_ENV).then(|| raw.clone());
        let cwd = std::env::current_dir().unwrap();
        let allowed = cwd.join("allowed/out.txt");
        for (target, decision) in [
            (allowed.to_str().unwrap(), "allow"),
            ("/allowed/out.txt", "deny"),
            ("allowed/out.txt", "allow"),
        ] {
            let stdin = serde_json::json!({"tool_name":"Bash", "tool_input":{
                "command":format!("echo x > {target}")}, "cwd":"", "permission_mode":"default"})
            .to_string();
            let mut out = Vec::new();
            assert_eq!(
                run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &env).unwrap(),
                0
            );
            let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(
                value["hookSpecificOutput"]["permissionDecision"], decision,
                "{target}"
            );
        }
    }

    #[test]
    fn argv_check_applies_the_same_envelope_as_explain() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let mut envelope = envelope::WorkerEnvelope::locked();
        envelope.paths = vec![envelope::PathScope::new("allowed")];
        envelope.tools = envelope::ToolSet::all();
        envelope.expires_at = u64::MAX;
        let raw = serde_json::to_string(&envelope).unwrap();
        let env = |key: &str| (key == super::super::agent::ENVELOPE_ENV).then(|| raw.clone());
        let args = CheckArgs {
            repo: repo.path().to_path_buf(),
            mode: LaunchMode::Interactive,
            command: vec!["echo blocked > outside.txt".into()],
            agent: None,
        };
        let mut out = Vec::new();
        assert_eq!(run_check(&args, &mut out, &env).unwrap(), 2);
        assert!(String::from_utf8(out).unwrap().contains("<envelope:"));
        let args = ExplainArgs {
            repo: args.repo,
            mode: args.mode,
            command: args.command,
        };
        let mut out = Vec::new();
        run_explain(&args, &mut out, &env).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("<envelope:"));
    }

    /// `run_check`'s hook-mode branch delegates to `HookToolPayload::parse`
    /// plus `evaluate`/`hook_output`, both already covered directly below;
    /// this pins the parse/filter half specifically -- a non-`Bash` tool or
    /// an empty command must read as "nothing to check", not an error.
    #[test]
    fn hook_payload_parsing_skips_non_bash_tools_and_empty_commands() {
        let payload =
            HookToolPayload::parse(r#"{"tool_name":"Read","tool_input":{}}"#).expect("parses");
        assert_eq!(payload.tool_name, "Read");
        assert_eq!(payload.tool_input.command, "");

        let payload =
            HookToolPayload::parse(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#)
                .expect("parses");
        assert_eq!(payload.tool_name, "Bash");
        assert_eq!(payload.tool_input.command, "ls");

        assert!(HookToolPayload::parse("not json").is_none());
    }

    #[test]
    fn powershell_tool_commands_use_the_same_cross_platform_policy_hook() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let cfg = CtxConfig::load(repo.path(), &|_| None).expect("loads");
        let stdin = r#"{"tool_name":"PowerShell","tool_input":{"command":"Remove-Item C:\\\\work -Recurse"},"permission_mode":"default"}"#;
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("\"permissionDecision\":\"ask\""),
            "PowerShell must not bypass the command guard on native Windows: {text}"
        );
    }

    #[test]
    fn hook_output_is_silent_for_allow_under_dont_ask_and_names_other_decisions() {
        let allow = Outcome {
            verdict: Verdict::Allow,
            matched: None,
        };
        assert!(
            hook_output(
                "ls",
                &allow,
                "dontAsk",
                SnapshotDivergence::Unchanged,
                "not-present"
            )
            .is_none()
        );

        let deny = Outcome {
            verdict: Verdict::Deny,
            matched: Some(Rule {
                pattern: "rm -rf *".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        let output = hook_output(
            "rm -rf /",
            &deny,
            "default",
            SnapshotDivergence::Unchanged,
            "not-present",
        )
        .expect("deny produces output");
        assert!(output.contains("\"permissionDecision\":\"deny\""));
        assert!(output.contains("\"hookEventName\":\"PreToolUse\""));

        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "git push*".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        let output = hook_output(
            "git push",
            &ask,
            "default",
            SnapshotDivergence::Unchanged,
            "not-present",
        )
        .expect("ask produces output");
        assert!(output.contains("\"permissionDecision\":\"ask\""));
    }

    // -- dontAsk fall-through (issue #102) -------------------------------
    //
    // A hook "ask" is an unsatisfiable prompt under `--permission-mode
    // dontAsk` (claude treats it as "deny if not pre-approved"), so it must
    // fall through and emit nothing rather than strip the operator's own
    // `permissions.allow` entries. `Deny` still denies in every mode, and
    // every mode other than `dontAsk` keeps emitting `"ask"`.

    #[test]
    fn hook_output_ask_under_dont_ask_falls_through_to_nothing() {
        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "git push*".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        assert!(
            hook_output(
                "git push",
                &ask,
                "dontAsk",
                SnapshotDivergence::Unchanged,
                "not-present"
            )
            .is_none()
        );
    }

    #[test]
    fn hook_output_deny_still_denies_under_dont_ask() {
        let deny = Outcome {
            verdict: Verdict::Deny,
            matched: Some(Rule {
                pattern: "rm -rf *".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        let output = hook_output(
            "rm -rf /",
            &deny,
            "dontAsk",
            SnapshotDivergence::Unchanged,
            "not-present",
        )
        .expect("deny still denies");
        assert!(output.contains("\"permissionDecision\":\"deny\""));
    }

    /// Change 5b: a `Deny` must carry the family and the `BLOCKED:`
    /// instruction so a worker knows to report the block rather than retry
    /// or work around it -- but the deliberately silent `Ask`-under-
    /// `dontAsk` path (issue #102's own finding) must still emit nothing at
    /// all, never leak the instruction through some other channel.
    #[test]
    fn deny_reason_carries_the_blocked_instruction_but_the_silent_ask_path_stays_silent() {
        let deny = Outcome {
            verdict: Verdict::Deny,
            matched: Some(Rule {
                pattern: "rm -rf *".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        let output = hook_output(
            "rm -rf",
            &deny,
            "default",
            SnapshotDivergence::Unchanged,
            "not-present",
        )
        .expect("deny produces output");
        assert!(
            output.contains("BLOCKED: rm"),
            "the family must be named in the reason: {output}"
        );
        assert!(
            output.contains("report") && output.contains("BLOCKED:"),
            "the reason must instruct the worker to report the block: {output}"
        );

        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "git push*--force*".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        assert!(
            hook_output(
                "git push --force x",
                &ask,
                "dontAsk",
                SnapshotDivergence::Unchanged,
                "not-present",
            )
            .is_none(),
            "the unsatisfiable-prompt-under-dontAsk case must stay silent, BLOCKED text or not"
        );
    }

    #[test]
    fn hook_output_ask_with_no_permission_mode_still_asks() {
        // Backward compatible: an older claude CLI (or any payload that
        // omits `permission_mode`) parses to the empty string default, which
        // must not be treated as `dontAsk`.
        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "git push*".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        let output = hook_output(
            "git push",
            &ask,
            "",
            SnapshotDivergence::Unchanged,
            "not-present",
        )
        .expect("ask still asks");
        assert!(output.contains("\"permissionDecision\":\"ask\""));
    }

    #[test]
    fn hook_tool_payload_parses_the_permission_mode_field() {
        let payload = HookToolPayload::parse(
            r#"{"tool_name":"Bash","tool_input":{"command":"ls"},"permission_mode":"dontAsk"}"#,
        )
        .expect("parses");
        assert_eq!(payload.permission_mode, "dontAsk");

        // Absent entirely: defaults to empty, not an error.
        let payload =
            HookToolPayload::parse(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#)
                .expect("parses");
        assert_eq!(payload.permission_mode, "");
    }

    #[test]
    fn hook_tool_payload_parses_the_unsandboxed_retry_marker() {
        let payload = HookToolPayload::parse(
            r#"{"tool_name":"Bash","tool_input":{"command":"make release","dangerouslyDisableSandbox":true},"permission_mode":"default"}"#,
        )
        .expect("parses");
        assert!(payload.tool_input.dangerously_disable_sandbox);

        let ordinary =
            HookToolPayload::parse(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#)
                .expect("parses");
        assert!(!ordinary.tool_input.dangerously_disable_sandbox);
    }

    #[test]
    fn run_check_hook_mode_auto_without_pin_allows_unmatched_command() {
        let cfg = CtxConfig::default();
        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":"auto"}"#;
        let mut out = Vec::new();
        assert_eq!(
            run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|_| None).unwrap(),
            0
        );
        let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn run_check_hook_mode_bypass_permissions_without_pin_allows_unmatched_command() {
        let cfg = CtxConfig::default();
        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":"bypassPermissions"}"#;
        let mut out = Vec::new();
        assert_eq!(
            run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|_| None).unwrap(),
            0
        );
        let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn run_check_hook_mode_future_permission_modes_are_interactive() {
        let cfg = CtxConfig::default();
        for mode in ["default", "plan", "acceptEdits", "futureMode"] {
            let stdin = serde_json::json!({
                "tool_name": "Bash",
                "tool_input": {"command": "xcrun simctl list"},
                "permission_mode": mode,
            })
            .to_string();
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, &stdin, &|_| None).unwrap();
            let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(
                output["hookSpecificOutput"]["permissionDecision"], "allow",
                "{mode}"
            );
        }
    }

    #[test]
    fn run_check_hook_mode_dont_ask_without_pin_audits_ask_but_stays_silent() {
        let cfg = CtxConfig::default();
        let state = tempfile::tempdir().unwrap();
        let env = |key: &str| {
            (key == super::super::state::STATE_ENV)
                .then(|| state.path().to_string_lossy().into_owned())
        };
        let stdin = r#"{"session_id":"unmatched-dont-ask","tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":"dontAsk"}"#;
        let mut out = Vec::new();
        assert_eq!(
            run_check_hook_mode_with_env(&cfg, &mut out, stdin, &env).unwrap(),
            0
        );
        assert!(out.is_empty(), "dontAsk must keep its silent ask: {out:?}");
        let path = std::fs::read_dir(state.path().join("logs/safety-decisions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let audit: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(audit["mode"], "headless");
        assert_eq!(audit["verdict"], "ask");
        assert!(audit["matched_pattern"].is_null());
    }

    #[test]
    fn run_check_hook_mode_empty_permission_mode_is_headless() {
        let cfg = CtxConfig::default();
        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":""}"#;
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|_| None).unwrap();
        let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "ask");
        let reason = output["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("headless default (ask)"), "{reason}");
        assert!(!reason.contains("interactive default"), "{reason}");
    }

    #[test]
    fn run_check_hook_mode_reason_names_the_mode_that_decided() {
        let mut cfg = CtxConfig::default();
        // An explicit headless denial emits a reason even under dontAsk.
        cfg.safety.default = Verdict::Deny;
        for (stdin, expected) in [
            (
                r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":"dontAsk"}"#,
                "headless default (deny)",
            ),
            (
                r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":"auto"}"#,
                "interactive default (allow)",
            ),
        ] {
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|_| None).unwrap();
            let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
            let reason = output["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap();
            assert!(reason.contains(expected), "{reason}");
            if stdin.contains("dontAsk") {
                assert!(!reason.contains("interactive default"), "{reason}");
            }
        }
    }

    #[test]
    fn run_check_hook_mode_denied_command_denies_under_both_modes() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        for mode in ["dontAsk", "default"] {
            let stdin = format!(
                r#"{{"tool_name":"Bash","tool_input":{{"command":"cat ~/.ssh/id_rsa"}},"permission_mode":"{mode}"}}"#
            );
            let mut out = Vec::new();
            let code = run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
            assert_eq!(code, 0);
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains("\"permissionDecision\":\"deny\""),
                "mode {mode}: got {text}"
            );
        }
    }

    #[test]
    // Finding 7 (2026-08-24 review): this test's own name and assertion used
    // to encode the vulnerability the finding describes -- an absent
    // `permission_mode` was treated as proof of an interactive, human-
    // attended session and got the permissive `allow` default. Renamed and
    // re-asserted for the corrected, fail-closed behavior.
    fn run_check_hook_mode_missing_permission_mode_fails_closed_to_headless() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");
        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"some-unknown-tool --flag"}}"#;
        let mut out = Vec::new();
        let code = run_check_hook_mode(&cfg, &mut out, stdin).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\"permissionDecision\":\"ask\""),
            "got {text}"
        );
    }

    #[test]
    fn run_check_hook_mode_interactive_pin_overrides_missing_mode_and_dont_ask() {
        let cfg = CtxConfig::default();
        let env = |key: &str| {
            (key == super::super::adapters::LAUNCH_MODE_ENV)
                .then(|| super::super::adapters::LAUNCH_MODE_INTERACTIVE_VALUE.to_string())
        };
        for stdin in [
            r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"}}"#,
            r#"{"tool_name":"Bash","tool_input":{"command":"xcrun simctl list"},"permission_mode":"dontAsk"}"#,
        ] {
            let mut out = Vec::new();
            run_check_hook_mode_with_env(&cfg, &mut out, stdin, &env).unwrap();
            if stdin.contains("dontAsk") {
                assert!(out.is_empty(), "dontAsk allow must remain silent");
                let mut cfg = cfg.clone();
                cfg.safety.interactive_default = Verdict::Deny;
                run_check_hook_mode_with_env(&cfg, &mut out, stdin, &env).unwrap();
                let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
                assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
                let reason = output["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap();
                assert!(reason.contains("interactive default (deny)"), "{reason}");
            } else {
                let output: serde_json::Value = serde_json::from_slice(&out).unwrap();
                assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "allow");
                let reason = output["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap();
                assert!(reason.contains("interactive default (allow)"), "{reason}");
            }
        }
    }

    #[test]
    fn three_consecutive_denials_in_one_session_flip_the_reason_on_the_third() {
        let cfg = default_cfg_for_loop_breaker_tests();
        assert_eq!(cfg.safety.denial_breaker_threshold, 3);
        let state = tempfile::tempdir().expect("state");
        let command = "git push --force origin main";

        let first = run_hook_for_loop_breaker(&cfg, state.path(), "brk-1", command, "default");
        assert!(
            first.contains(r#""permissionDecision":"ask""#),
            "got {first}"
        );
        assert!(
            !first.contains("consecutive denials"),
            "1st denial must not trip the breaker: {first}"
        );

        let second = run_hook_for_loop_breaker(&cfg, state.path(), "brk-1", command, "default");
        assert!(
            !second.contains("consecutive denials"),
            "2nd denial must not trip the breaker: {second}"
        );

        let third = run_hook_for_loop_breaker(&cfg, state.path(), "brk-1", command, "default");
        assert!(
            third.contains("3 consecutive denials this session"),
            "3rd denial must trip the breaker: {third}"
        );
        assert!(
            third.contains(" -- ") && third.contains("git push"),
            "the original policy explanation must survive after ` -- `: {third}"
        );
    }

    #[test]
    fn an_allow_between_denials_resets_the_breakers_trailing_run() {
        let cfg = default_cfg_for_loop_breaker_tests();
        let state = tempfile::tempdir().expect("state");
        let deny_command = "git push --force origin main";
        let allow_command = "npm install";

        run_hook_for_loop_breaker(&cfg, state.path(), "brk-2", deny_command, "default");
        run_hook_for_loop_breaker(&cfg, state.path(), "brk-2", deny_command, "default");
        let allowed =
            run_hook_for_loop_breaker(&cfg, state.path(), "brk-2", allow_command, "default");
        assert!(
            allowed.contains(r#""permissionDecision":"allow""#),
            "got {allowed}"
        );

        let third_deny =
            run_hook_for_loop_breaker(&cfg, state.path(), "brk-2", deny_command, "default");
        assert!(
            !third_deny.contains("consecutive denials"),
            "the intervening allow must reset the trailing run: {third_deny}"
        );
    }

    #[test]
    fn a_zero_denial_breaker_threshold_never_trips() {
        let mut cfg = default_cfg_for_loop_breaker_tests();
        cfg.safety.denial_breaker_threshold = 0;
        let state = tempfile::tempdir().expect("state");
        let command = "git push --force origin main";
        for _ in 0..5 {
            let output = run_hook_for_loop_breaker(&cfg, state.path(), "brk-3", command, "default");
            assert!(
                !output.contains("consecutive denials"),
                "threshold 0 must disable the breaker entirely: {output}"
            );
        }
    }

    #[test]
    fn a_different_sessions_denials_never_count_towards_this_sessions_breaker() {
        let cfg = default_cfg_for_loop_breaker_tests();
        let state = tempfile::tempdir().expect("state");
        let command = "git push --force origin main";
        run_hook_for_loop_breaker(&cfg, state.path(), "sess-a", command, "default");
        run_hook_for_loop_breaker(&cfg, state.path(), "sess-a", command, "default");
        run_hook_for_loop_breaker(&cfg, state.path(), "sess-b", command, "default");
        let second_for_b =
            run_hook_for_loop_breaker(&cfg, state.path(), "sess-b", command, "default");
        assert!(
            !second_for_b.contains("consecutive denials"),
            "sess-b has only 2 of its own denials, sess-a's must not count: {second_for_b}"
        );
    }

    #[test]
    fn trailing_same_command_failure_run_counts_only_the_trailing_run_of_this_exact_command() {
        let jsonl = transcript_jsonl(&[
            ("b1", "cargo test", true),
            ("b2", "cargo test", true),
            ("b3", "cargo test", false),
            ("b4", "cargo test", true),
            ("other", "cargo build", true),
        ]);
        assert_eq!(
            trailing_same_command_failure_run(&jsonl, "cargo test"),
            1,
            "the success at b3 resets the run; a different command never counts"
        );
        assert_eq!(trailing_same_command_failure_run(&jsonl, "cargo build"), 1);
    }

    #[test]
    fn trailing_same_command_failure_run_skips_unparseable_lines() {
        let mut jsonl = transcript_jsonl(&[("b1", "cargo test", true), ("b2", "cargo test", true)]);
        jsonl.push_str("not json\n");
        assert_eq!(trailing_same_command_failure_run(&jsonl, "cargo test"), 2);
    }

    #[test]
    fn two_trailing_failures_warn_but_still_allow() {
        let cfg = default_cfg_for_loop_breaker_tests();
        assert_eq!(cfg.safety.identical_command_warn_after, 2);
        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl = transcript_jsonl(&[("b1", "cargo test", true), ("b2", "cargo test", true)]);
        let path = write_transcript(&dir, &jsonl);

        let output = run_hook_with_transcript(&cfg, "cargo test", "default", &path);
        assert!(
            output.contains(r#""permissionDecision":"allow""#),
            "got {output}"
        );
        assert!(
            output.contains("additionalContext")
                && output.contains("failed 2 times in a row")
                && output.contains("change the approach"),
            "got {output}"
        );
    }

    #[test]
    fn five_trailing_failures_refuse_headless_but_only_warn_interactive() {
        let cfg = default_cfg_for_loop_breaker_tests();
        assert_eq!(cfg.safety.identical_command_refuse_after, 5);
        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl = transcript_jsonl(&[
            ("b1", "cargo test", true),
            ("b2", "cargo test", true),
            ("b3", "cargo test", true),
            ("b4", "cargo test", true),
            ("b5", "cargo test", true),
        ]);
        let path = write_transcript(&dir, &jsonl);

        let headless = run_hook_with_transcript(&cfg, "cargo test", "dontAsk", &path);
        assert!(
            headless.contains(r#""permissionDecision":"deny""#),
            "headless must refuse at the 5th trailing failure: {headless}"
        );
        assert!(
            headless.contains("refusing to run it again"),
            "got {headless}"
        );

        let interactive = run_hook_with_transcript(&cfg, "cargo test", "default", &path);
        assert!(
            interactive.contains(r#""permissionDecision":"allow""#),
            "interactive must never auto-refuse: {interactive}"
        );
        assert!(
            interactive.contains("additionalContext") && interactive.contains("failed 5 times"),
            "interactive still warns: {interactive}"
        );
    }

    #[test]
    fn a_read_only_command_never_triggers_the_identical_command_guard() {
        let cfg = default_cfg_for_loop_breaker_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl = transcript_jsonl(&[
            ("b1", "git status", true),
            ("b2", "git status", true),
            ("b3", "git status", true),
            ("b4", "git status", true),
            ("b5", "git status", true),
        ]);
        let path = write_transcript(&dir, &jsonl);

        let output = run_hook_with_transcript(&cfg, "git status", "dontAsk", &path);
        assert!(
            !output.contains("additionalContext") && !output.contains("zirv guard"),
            "a read-only command must never be guarded, even headless: {output}"
        );
    }

    #[test]
    fn a_success_in_the_middle_resets_the_guards_trailing_run_too() {
        let cfg = default_cfg_for_loop_breaker_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl = transcript_jsonl(&[
            ("b1", "cargo test", true),
            ("b2", "cargo test", true),
            ("b3", "cargo test", false),
        ]);
        let path = write_transcript(&dir, &jsonl);

        let output = run_hook_with_transcript(&cfg, "cargo test", "default", &path);
        assert!(
            !output.contains("additionalContext"),
            "the success at b3 must reset the trailing run below warn_after: {output}"
        );
    }
}
