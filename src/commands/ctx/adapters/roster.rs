//! Harness roster/readiness reporting: per-adapter prompt lines, capability
//! gaps, and the readiness note.
use super::*;

/// Build the orchestrator's delegation roster from enabled, ready harnesses; uncertain presence stays visible, confirmed absence is omitted. The current harness cannot delegate to itself. (#298)
/// Rendered only for an Orchestrator prompt, never a Worker's: a worker must not learn what it
/// could delegate to.
#[cfg(test)]
pub fn harness_prompt_lines(cfg: &CtxConfig, current_adapter: &str) -> Vec<String> {
    harness_roster_lines(cfg, current_adapter, None).lines
}

/// As [`harness_prompt_lines`], but reusing (and updating) `cache`'s probe
/// verdicts instead of probing fresh on every call -- `compile::compile_
/// with_harness_roster`'s own entry point. See [`ProbeCache`]'s own doc
/// comment for the scope/TTL it substitutes for a literal per-session cache.
pub fn harness_prompt_lines_cached(
    cfg: &CtxConfig,
    current_adapter: &str,
    cache: &mut ProbeCache,
) -> HarnessRosterReport {
    harness_roster_lines(cfg, current_adapter, Some(cache))
}

/// The per-adapter roster lines both entry points above share, plus issue
/// #298's own measurement data: how many adapter lines were omitted for a
/// confirmed-`Absent` `Liveness` (or a disabled adapter), and how many bytes
/// those lines would have cost had they still been rendered the pre-#298 way
/// (`"- name: disabled (...)"` / `"- name: not installed (...)"`) -- the
/// success metric `zirv ctx compile --measure` reports.
pub struct HarnessRosterReport {
    pub lines: Vec<String>,
    pub omitted: usize,
    pub omitted_bytes: usize,
}

/// Share adapter construction and presence verdicts across prompt and chat rosters; callers apply their own enabled gate.
pub(crate) fn adapter_liveness(
    cfg: &CtxConfig,
    name: &str,
    cache: Option<&mut ProbeCache>,
) -> Result<(Box<dyn AgentAdapter>, Liveness), String> {
    adapter_liveness_with(cfg, name, cache, &liveness_probe)
}

/// [`adapter_liveness`] with an injected presence oracle, so a caller can state the machine it reasons about.
pub(crate) fn adapter_liveness_with(
    cfg: &CtxConfig,
    name: &str,
    cache: Option<&mut ProbeCache>,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> Result<(Box<dyn AgentAdapter>, Liveness), String> {
    let bin = cfg.agent_bin.as_deref();
    let Some((_, ctor)) = ADAPTERS.iter().find(|(n, _)| *n == name) else {
        return Err(format!("no adapter registered for '{name}'"));
    };
    let names_other = agent_bin_names_a_different_adapter(bin, name).is_some();
    let mut adapter = if names_other { ctor(None) } else { ctor(bin) };
    apply_endpoint_override(&mut adapter, cfg);
    apply_chat_override(&mut adapter, cfg);
    apply_headless_override(&mut adapter, cfg);
    if let Err(err) = adapter.ready() {
        // A harness that can never run on this platform is confirmed absent, not uncertain.
        if adapter.platform_unsupported() {
            return Ok((adapter, Liveness::Absent(err.to_string())));
        }
        return Err(err.to_string());
    }
    let program = adapter.program().to_string();
    let resolved_bin = if names_other { None } else { bin };
    let verdict = match cache {
        Some(cache) => {
            let key = ProbeCache::key(name, &program, resolved_bin);
            cache.get_or_probe(&key, || present(name, &program))
        }
        None => present(name, &program),
    };
    Ok((adapter, verdict))
}

fn harness_roster_lines(
    cfg: &CtxConfig,
    current_adapter: &str,
    mut cache: Option<&mut ProbeCache>,
) -> HarnessRosterReport {
    let mut lines: Vec<String> = Vec::new();
    let mut omitted_texts: Vec<String> = Vec::new();
    // Issue #298 review follow-up: every name that earns its own per-adapter
    // line above (ready-and-live, ready-and-unknown/fail-open, or ready()
    // itself failed) -- never a disabled or confirmed-`Absent` one. Threaded
    // into `review_roster_line` below so it never directs a review to a
    // harness the roster above it just omitted, without a second probe: the
    // liveness verdict already computed (and cached) in this same loop is
    // reused, not recomputed, so the injected prefix stays byte-stable.
    let mut roster_names: Vec<&str> = Vec::new();
    for (name, _) in ADAPTERS {
        let name: &str = name;
        let is_self = name == current_adapter;
        let (enabled, location) = cfg
            .agents
            .states()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| (s.enabled, s.location()))
            .unwrap_or((true, "default".to_string()));
        if !enabled {
            // Issue #298: absence, not annotation -- `location` is recorded
            // only for `compile --measure`'s own note column, never for a
            // session's own prompt.
            omitted_texts.push(format!("- {name}: disabled ({location})"));
            continue;
        }

        match adapter_liveness(cfg, name, cache.as_deref_mut()) {
            Ok((adapter, verdict)) => {
                if let Liveness::Unknown(reason) = &verdict {
                    eprintln!(
                        "zirv ctx: liveness probe for '{name}' inconclusive ({reason}); \
                         treating it as live rather than costing the session a capability \
                         it might actually have"
                    );
                }
                if !verdict.emits_line() {
                    let program = adapter.program();
                    omitted_texts.push(format!("- {name}: not installed (no '{program}' found)"));
                    continue;
                }
                let missing = missing_capability_labels(adapter.capabilities());
                let degraded = if missing.is_empty() {
                    String::new()
                } else {
                    format!(" (degraded: no {})", join_with_or(&missing))
                };
                // Repo `.settings.toml` (or the operator, or the
                // environment) may mark a harness capacity-limited; the
                // roster line carries that forward so an orchestrator
                // routes only small, bounded briefs its way -- both for
                // reviews and for `zirv agent` delegations (see
                // `HARNESS_PROMPT`'s final paragraph).
                let capacity_note = if cfg.agents.is_capacity_small(name) {
                    " -- small tasks only"
                } else {
                    ""
                };
                // Issue #395: names an attached `[endpoint.<agent>]`
                // override by its catalogue vendor slug, never the URL or
                // credential -- `zirv ctx status`'s own `endpoint:` line
                // carries those.
                let endpoint_note = adapter
                    .endpoint_vendor()
                    .map(|vendor| format!(" (endpoint: {vendor})"))
                    .unwrap_or_default();
                lines.push(if is_self {
                    format!(
                        "- {name}: enabled, ready{capacity_note} (this session's harness){endpoint_note}{degraded}"
                    )
                } else {
                    format!(
                        "- {name}: enabled, ready{capacity_note} -- initiate with `zirv agent {name} \"<prompt>\"`{endpoint_note}{degraded}"
                    )
                });
                roster_names.push(name);
            }
            Err(reason) => {
                let short = reason.lines().next().unwrap_or(&reason);
                lines.push(format!("- {name}: installed? not ready ({short})"));
                roster_names.push(name);
            }
        }
    }
    if let Some(review_line) = review_roster_line(cfg, &roster_names) {
        lines.push(review_line);
    }
    let omitted = omitted_texts.len();
    let omitted_bytes = omitted_texts.iter().map(|line| line.len() + 1).sum();
    HarnessRosterReport {
        lines,
        omitted,
        omitted_bytes,
    }
}

/// The trailing "- code review: ..." line `harness_roster_lines` appends
/// after its per-harness lines: names every *enabled, roster-listed*
/// harness's resolved
/// review model (an operator override or the ladder default, each marked as
/// such) and states the rule that outranks any other model-routing guidance
/// a session's own base prompt carries; no other built-in layer restates the
/// review-model rule (#452).
/// Returns `None` when no harness is both enabled and roster-listed --
/// absence, not a line naming zero harnesses.
///
/// A disabled harness's entry is simply absent, the same "absence, not
/// silence" rule its own per-harness line above follows. Issue #298 review
/// follow-up: so is a confirmed-`Absent` one -- `roster_names` is the set of
/// adapter names that earned their own per-adapter line in this same pass
/// (`harness_roster_lines`'s own liveness verdict, already computed and
/// possibly cache-served, never re-probed here), and an entry is included
/// only when it is both enabled *and* in that set. Before issue #298 this
/// line checked only enablement -- "the rule applies to a harness the moment
/// it is enabled, whether or not its binary happens to be on disk" -- but
/// that reasoning stopped holding once the roster above it started omitting
/// a confirmed-absent adapter's own line: naming a harness here that the
/// roster never mentioned would send a review to something that cannot run.
/// A harness that is merely not-ready (`adapter.ready()` failed) still gets
/// its own "installed? not ready" line above, so it is still in
/// `roster_names` and still named here -- unchanged from before.
///
/// The rendered sentence must never be false for any entry. Two cases would
/// otherwise make it false: a harness whose ladder default is already at
/// the floor tier (seat "haiku" resolves claude's own default to "haiku"
/// too -- neither "one tier below the seat" nor "never on the seat's own
/// model" holds), and an operator who explicitly configures `review.<agent>`
/// equal to the seat (allowed -- the operator's choice wins -- but then
/// "never on the seat's own model" is false of that entry). Both are the
/// same underlying condition -- the resolved model's text equals the seat's
/// text, case-insensitively -- so both are detected by one `equals_seat`
/// check per entry, regardless of whether the model came from the ladder
/// default or an operator override. (Deliberately a plain text comparison,
/// not a second call into the ladder: re-running `review_model_below` on
/// the *resolved* model would also self-map at the floor tier for a seat
/// one rung *above* the floor -- e.g. seat "sonnet" resolves to "haiku",
/// and "haiku" maps to itself too -- which would wrongly flag a perfectly
/// true "one tier below the seat" note as a floor case.) When any entry's
/// `equals_seat` is true, the trailing clause softens from the strict
/// "never on an orchestrator seat's own model" to the weaker but always-true
/// "never on a model above the named one" (the named model is by
/// construction never ranked above the seat, so this holds in every case).
fn review_roster_line(cfg: &CtxConfig, roster_names: &[&str]) -> Option<String> {
    let bin = cfg.agent_bin.as_deref();
    let seat = cfg.chat.model.as_deref();
    let mut any_equals_seat = false;
    let entries: Vec<String> = ADAPTERS
        .iter()
        .filter(|(name, _)| cfg.agents.is_enabled(name) && roster_names.contains(name))
        .map(|(name, ctor)| {
            let mut adapter = if agent_bin_names_a_different_adapter(bin, name).is_some() {
                ctor(None)
            } else {
                ctor(bin)
            };
            // Apply endpoint override before choosing roster model, matching the actual review launch. (#395)
            apply_endpoint_override(&mut adapter, cfg);
            let choice = resolve_review_model(cfg, name, adapter.as_ref());
            let model = adapter.pin_model_for_endpoint(&choice.model);
            let equals_seat = seat.is_some_and(|s| s.eq_ignore_ascii_case(&model));
            if equals_seat {
                any_equals_seat = true;
            }
            let note = if choice.configured {
                "configured".to_string()
            } else if equals_seat {
                "floor tier: the seat is already at the bottom rung".to_string()
            } else {
                "default: one tier below the seat".to_string()
            };
            format!("{name} -> \"{}\" ({note})", model)
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let never_clause = if any_equals_seat {
        "never on a model above the named one"
    } else {
        "never on an orchestrator seat's own model"
    };
    Some(format!(
        "- code review: {} -- run every code review on the named model, {never_clause}. This \
         outranks any other model-routing guidance.",
        entries.join(", ")
    ))
}

/// A `Capabilities` predicate paired with its user-facing label -- factored
/// out purely to keep `CAPABILITY_LABELS`'s type simple enough for clippy's
/// `type_complexity` lint.
type CapabilityLabel = (fn(Capabilities) -> bool, &'static str);

/// The user-facing label for each `Capabilities` flag this disclosure cares
/// about, in a fixed reporting order. `marker_signal` is deliberately not
/// included: it is a sub-feature of `events` (no event parsing means no
/// marker detection either), so listing both would say the same thing twice.
const CAPABILITY_LABELS: &[CapabilityLabel] = &[
    (|c| c.events, "rot score"),
    (|c| c.token_usage, "usage"),
    (|c| c.turn_signal, "turn signal"),
    (|c| c.system_prompt, "injected prompt"),
];

/// Which of [`CAPABILITY_LABELS`] this adapter's `capabilities()` reports as
/// missing, in the same fixed order.
fn missing_capability_labels(caps: Capabilities) -> Vec<&'static str> {
    CAPABILITY_LABELS
        .iter()
        .filter(|(has, _)| !has(caps))
        .map(|(_, label)| *label)
        .collect()
}

/// `["a", "b", "c"]` -> `"a, b, or c"`; `["a", "b"]` -> `"a or b"`; `["a"]` ->
/// `"a"`. Plain English list join for a short, human-readable sentence.
fn join_with_or(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [first, second] => format!("{first} or {second}"),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty, matched above");
            format!("{}, or {last}", rest.join(", "))
        }
    }
}

/// Report absent, permanently unsupported, and capability-degraded adapters separately without asserting uncertain absence.
pub(crate) static READINESS_NOTE_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn readiness_note() -> String {
    READINESS_NOTE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut clauses: Vec<String> = Vec::new();

    // Construct and check each adapter once per roster build; readiness probes may be expensive.
    let mut not_ready: Vec<&str> = Vec::new();
    let mut unsupported: Vec<&str> = Vec::new();
    let mut degraded: Vec<String> = Vec::new();
    for (name, ctor) in ADAPTERS {
        let adapter = ctor(None);
        if adapter.ready().is_err() {
            if adapter.platform_unsupported() {
                unsupported.push(name);
            } else {
                not_ready.push(name);
            }
            continue;
        }
        let missing = missing_capability_labels(adapter.capabilities());
        if !missing.is_empty() {
            degraded.push(format!(
                "{name} (launch-level: no {})",
                join_with_or(&missing)
            ));
        }
    }

    if !not_ready.is_empty() {
        clauses.push(format!(
            "Not ready yet: {} (see issue #11).",
            not_ready.join(", ")
        ));
    }
    if !unsupported.is_empty() {
        clauses.push(format!(
            "Unsupported on this platform: {}.",
            unsupported.join(", ")
        ));
    }
    if !degraded.is_empty() {
        clauses.push(format!(
            "Degraded surface: {} (see issue #11).",
            degraded.join("; ")
        ));
    }

    clauses.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_with_or_reads_like_plain_english() {
        assert_eq!(join_with_or(&[]), "");
        assert_eq!(join_with_or(&["a"]), "a");
        assert_eq!(join_with_or(&["a", "b"]), "a or b");
        assert_eq!(join_with_or(&["a", "b", "c"]), "a, b, or c");
    }

    #[test]
    fn missing_capability_labels_names_only_the_false_flags() {
        let all_false = Capabilities::default();
        assert_eq!(
            missing_capability_labels(all_false),
            vec!["rot score", "usage", "turn signal", "injected prompt"]
        );

        let all_true = Capabilities {
            marker_signal: true,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
            events: true,
            defer_injection_submit: true,
            context_window_tokens: None,
            pre_tool_hook: true,
            post_tool_hook: true,
        };
        assert!(missing_capability_labels(all_true).is_empty());

        let mixed = Capabilities {
            events: true,
            ..Capabilities::default()
        };
        assert_eq!(
            missing_capability_labels(mixed),
            vec!["usage", "turn signal", "injected prompt"],
            "an adapter with real events but nothing else"
        );
    }

    /// F: codex is ready (its own `ready()` no longer hard-errors) but its
    /// usage/turn capabilities remain degraded, so `--help`'s about text
    /// must keep disclosing the degraded surface even though codex no longer
    /// shows up in the "not ready yet" clause at all. Issue #86 (2026-08-23)
    /// gave codex real event parsing, so "rot score" is no longer one of the
    /// missing labels -- this must NOT regress back to claiming codex has no
    /// rot score.
    #[test]
    fn the_readiness_note_discloses_codexs_degraded_surface_now_that_it_is_ready() {
        let note = readiness_note();
        assert!(
            !note.to_lowercase().contains("not ready"),
            "codex is ready now, not unready: {note}"
        );
        assert!(note.contains("codex"), "got {note}");
        assert!(note.contains("usage"), "got {note}");
        assert!(note.contains("turn signal"), "got {note}");
        // Codex's OWN clause must not claim the "injected prompt" gap --
        // `system_prompt_args` is real for codex (`-c
        // developer_instructions=...`). Gemini (added #384) legitimately
        // carries that gap (`GEMINI_SYSTEM_MD` is env-var-only, no per-run
        // argv mechanism), so this checks codex's own exact clause rather
        // than asserting the whole note never mentions the phrase at all.
        // Issue #86 gave codex real event parsing, so its own clause must not
        // claim "no rot score" either -- wave 3's grok/kimi/cursor-agent/
        // goose/muse (issues #390-#394) legitimately carry that gap (no
        // verified row-level transcript schema for any of them), so this
        // checks codex's own exact clause rather than asserting the whole
        // note never mentions the phrase at all.
        assert!(
            note.contains("codex (launch-level: no usage or turn signal)"),
            "got {note}"
        );
        assert!(note.contains("issue #11"), "got {note}");
        assert!(
            !note.contains("claude (launch-level"),
            "claude is fully capable and must not appear in the degraded clause: {note}"
        );
    }

    /// Issue #394 review follow-up: on Windows, muse's `ready()` always
    /// fails (see `MuseAdapter::ready`), but that failure is a permanent
    /// platform fact, not a "not installed yet" one -- it must land in its
    /// own "Unsupported on this platform" clause, never in "Not ready yet"
    /// (which reads as "go install this" and would be false for muse here).
    /// This runs against the real `ADAPTERS` table with no rigging, since
    /// muse's Windows refusal is unconditional.
    #[cfg(windows)]
    #[test]
    fn readiness_note_files_muse_as_platform_unsupported_not_not_ready_on_windows() {
        let note = readiness_note();
        assert!(note.contains("Unsupported on this platform"), "got {note}");
        assert!(note.contains("muse"), "got {note}");
        assert!(
            !note.to_lowercase().contains("not ready"),
            "muse's Windows refusal is permanent, not a fixable not-ready state: {note}"
        );
    }

    /// A harness that can never run on this platform is confirmed absent, so
    /// the roster omits it instead of rendering "installed? not ready".
    #[cfg(windows)]
    #[test]
    fn a_platform_unsupported_harness_is_absent_from_the_roster_on_windows() {
        let cfg = super::super::tests::permissive_cfg();
        let verdict = adapter_liveness(&cfg, "muse", None).map(|(_, verdict)| verdict);
        assert!(
            matches!(verdict, Ok(Liveness::Absent(_))),
            "got {verdict:?}"
        );
        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            !lines.iter().any(|l| l.starts_with("- muse:")),
            "got {lines:?}"
        );
    }

    /// Symmetry check for the test immediately above: off Windows, muse's
    /// `ready()` succeeds (nothing on this codebase requires the binary to
    /// actually be installed -- see `resolve_program`'s non-Windows arm), so
    /// it must never appear in either the "Unsupported on this platform" or
    /// "Not ready yet" clause -- only (legitimately, per issue #394) in
    /// "Degraded surface", alongside every other wave-3 adapter.
    #[cfg(not(windows))]
    #[test]
    fn readiness_note_never_files_muse_as_unsupported_or_not_ready_off_windows() {
        let note = readiness_note();
        assert!(!note.contains("Unsupported on this platform"), "got {note}");
        assert!(!note.to_lowercase().contains("not ready"), "got {note}");
        assert!(
            note.contains("muse (launch-level"),
            "muse is ready but still degraded (issue #394): {note}"
        );
    }

    /// Issue #298: a `Live` adapter (its own default program name genuinely
    /// present, here via a restricted `PATH` carrying a stub for each --
    /// `permissive_cfg`/`CtxConfig::default` alone says nothing about the
    /// real machine's `PATH`, so this test must not depend on it) earns a
    /// line; an `Absent` one would not, so pinning "one line per adapter" to
    /// mean anything under this issue's omission rule requires every
    /// adapter to be genuinely live first.
    #[test]
    fn harness_prompt_lines_returns_one_line_per_registered_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in ADAPTERS {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);

        let lines = harness_prompt_lines(&super::super::tests::permissive_cfg(), "");
        // One line per adapter this platform can run, plus one trailing
        // "- code review: ..." line naming every enabled harness's resolved
        // review model.
        let supported: Vec<&str> = ADAPTERS
            .iter()
            .filter(|(_, ctor)| !ctor(None).platform_unsupported())
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(lines.len(), supported.len() + 1);
        for name in supported {
            assert!(
                lines.iter().any(|l| l.starts_with(&format!("- {name}:"))),
                "missing a line for '{name}': {lines:?}"
            );
        }
        assert!(
            lines
                .last()
                .is_some_and(|l| l.starts_with("- code review:")),
            "the review line comes last: {lines:?}"
        );
    }

    /// Unconfigured `review.claude`/`review.codex`: the roster line names
    /// each enabled harness's ladder-computed default (one tier below the
    /// seat -- unset `chat.model` assumes the top tier), marks each entry as
    /// a default rather than an operator choice, and states the never-the-
    /// seat / outranks-other-routing rule.
    #[test]
    fn harness_prompt_lines_review_line_shows_computed_defaults_when_unset() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let lines = harness_prompt_lines(&super::super::tests::permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("outranks"),
            "states it outranks other routing guidance: {review_line}"
        );
    }

    /// An operator-configured `review.<agent>` wins over the ladder default
    /// and is marked `(configured)` rather than `(default: ...)`.
    #[test]
    fn harness_prompt_lines_review_line_uses_the_operators_configured_model() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let cfg = CtxConfig {
            review: crate::commands::ctx::config::ReviewConfig {
                claude: Some("custom-review-model".to_string()),
                codex: None,
            },
            ..super::super::tests::permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"custom-review-model\" (configured)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "codex stays on its computed default: {review_line}"
        );
    }

    /// A disabled harness gets no entry in the review line at all -- same
    /// absence-not-silence rule its own per-harness line above follows.
    #[test]
    fn harness_prompt_lines_review_line_omits_a_disabled_harnesses_entry() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let cfg = super::super::tests::cfg_disabling("codex");
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude ->"),
            "claude stays: {review_line}"
        );
        assert!(
            !review_line.contains("codex ->"),
            "a disabled harness must not appear in the review line: {review_line}"
        );
    }

    /// Review follow-up on issue #298: an adapter the roster above just
    /// omitted for being confirmed `Absent` must not still be named in the
    /// trailing review line either -- that would send a review to a harness
    /// that cannot run, the one inconsistency the roster's own omission
    /// rule exists to prevent. Claude stays live via a real stub on a `PATH`
    /// that carries no `codex` binary and no known install root for it
    /// either (an isolated `HOME`), so codex is confirmed absent, not
    /// merely undetected.
    #[test]
    fn harness_prompt_lines_review_line_omits_a_confirmed_absent_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude"), "").expect("write stub");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let lines = harness_prompt_lines(&super::super::tests::permissive_cfg(), "");
        let review_line = lines
            .iter()
            .find(|l| l.starts_with("- code review:"))
            .expect("claude is live, so the review line must still exist");
        assert!(review_line.contains("claude ->"), "got {review_line}");
        assert!(
            !review_line.contains("codex ->"),
            "codex is confirmed absent, so it must not be named in the review line: \
             {review_line}"
        );
    }

    /// Positive case: with both adapters genuinely live, the liveness filter
    /// above changes nothing -- the review line is exactly the text it
    /// rendered before this fix existed (same substrings `harness_prompt_
    /// lines_review_line_shows_computed_defaults_when_unset` already pins,
    /// here with an explicit `PATH` stub so the claim holds independent of
    /// whatever the machine running this suite happens to have installed).
    #[test]
    fn harness_prompt_lines_review_line_is_unchanged_when_both_adapters_are_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in ADAPTERS {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);

        let lines = harness_prompt_lines(&super::super::tests::permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("codex -> \"gpt-5.6-terra\" (default: one tier below the seat)"),
            "got {review_line}"
        );
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            review_line.ends_with("This outranks any other model-routing guidance."),
            "got {review_line}"
        );
    }

    /// Normal case (no entry's resolved model equals the orchestrator seat):
    /// the strict "never on an orchestrator seat's own model" clause is
    /// true for every entry, so it stays.
    #[test]
    fn review_roster_line_normal_case_keeps_the_strict_clause() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let lines = harness_prompt_lines(&super::super::tests::permissive_cfg(), "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("never on an orchestrator seat's own model"),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("never on a model above the named one"),
            "the softened clause must not appear when nothing is contradictory: {review_line}"
        );
    }

    /// Floor-tier case: seat "haiku" resolves (unconfigured) to claude's own
    /// floor default "haiku" too -- neither "one tier below the seat" nor
    /// "never on an orchestrator seat's own model" would be true of that
    /// entry, so both the per-entry note and the global clause must adjust
    /// to stay honest.
    #[test]
    fn review_roster_line_floor_seat_case_is_not_contradictory() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("haiku".to_string()),
                claude_permission_mode: None,
            },
            ..super::super::tests::permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"haiku\""),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("claude -> \"haiku\" (default: one tier below the seat)"),
            "that note would be false when the seat is already the floor: {review_line}"
        );
        assert!(review_line.contains("floor tier"), "got {review_line}");
        assert!(
            !review_line.contains("never on an orchestrator seat's own model"),
            "that clause would be false for the floor-tier entry: {review_line}"
        );
    }

    /// Configured-equals-seat case: the operator's own `review.claude`
    /// explicitly names the same model as the orchestrator seat -- allowed
    /// (the operator's choice wins), but then "never on an orchestrator
    /// seat's own model" is false of that entry, so the global clause must
    /// soften to something that stays true.
    #[test]
    fn review_roster_line_configured_equals_seat_case_is_not_contradictory() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("opus".to_string()),
                claude_permission_mode: None,
            },
            review: crate::commands::ctx::config::ReviewConfig {
                claude: Some("opus".to_string()),
                codex: None,
            },
            ..super::super::tests::permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"opus\" (configured)"),
            "got {review_line}"
        );
        assert!(
            !review_line.contains("never on an orchestrator seat's own model"),
            "that clause would be false for the operator's own configured entry: {review_line}"
        );
    }

    /// Review finding: without `apply_endpoint_override` attached to the
    /// adapter this function itself constructs, an endpoint-overridden
    /// harness's roster line advised claude's native ladder text even
    /// though the actual review launch (`workflow::review::reviewer_args`,
    /// via this same adapter's own `model_args`) pins to the endpoint
    /// vendor's own ladder -- the two could name different models. Under
    /// `[endpoint.claude] vendor = "zhipu"` with no seat and no `review.
    /// claude` override, the roster line must name zhipu's own strongest
    /// rung (`"glm-5.3"`), never an Anthropic model.
    #[test]
    fn review_roster_line_names_the_endpoint_vendors_own_rung() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let cfg = CtxConfig {
            endpoint: crate::commands::ctx::config::EndpointConfig {
                claude: Some(crate::commands::ctx::config::EndpointTarget {
                    vendor: "zhipu".to_string(),
                    base_url: "https://api.z.ai/api/anthropic".to_string(),
                    credential_env: "ZIRV_TEST_UNUSED_395".to_string(),
                    model: None,
                    wire_api: None,
                }),
                codex: None,
            },
            ..super::super::tests::permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"glm-5.3\""),
            "the roster line must name the endpoint vendor's own rung, not the native ladder: \
             got {review_line}"
        );
        for native in ["opus", "sonnet", "haiku", "fable", "mythos"] {
            assert!(
                !review_line.contains(&format!("claude -> \"{native}\"")),
                "must never advise claude's native ladder for an endpoint-overridden harness: \
                 got {review_line}"
            );
        }
    }

    /// The seat threads all the way from `cfg.chat.model` through to the
    /// rendered claude entry: seat "sonnet" resolves claude's own ladder
    /// default to "haiku" (one tier below sonnet).
    #[test]
    fn harness_prompt_lines_review_line_threads_the_seat_for_claude() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        let cfg = CtxConfig {
            chat: crate::commands::ctx::config::ChatConfig {
                model: Some("sonnet".to_string()),
                claude_permission_mode: None,
            },
            ..super::super::tests::permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let review_line = lines.last().expect("at least the review line");
        assert!(
            review_line.contains("claude -> \"haiku\" (default: one tier below the seat)"),
            "got {review_line}"
        );
    }

    /// Issue #395, item 7: a harness with an attached `[endpoint.<agent>]`
    /// override earns an `(endpoint: <vendor>)` suffix on its own roster
    /// line -- never the base URL or the credential, which `zirv ctx
    /// status`'s own `endpoint:` line carries instead.
    #[test]
    fn harness_prompt_lines_names_an_attached_endpoint_override() {
        let _live = crate::commands::ctx::testenv::stub_live_adapters_on_path();
        // SAFETY (test): nextest isolates tests per process, and the serial
        // `cargo test -- --test-threads=1` run never overlaps this variable
        // with another test.
        unsafe {
            std::env::set_var("ZIRV_TEST_ROSTER_ZHIPU_KEY_395", "sekrit");
        }
        let cfg = CtxConfig {
            endpoint: crate::commands::ctx::config::EndpointConfig {
                claude: Some(crate::commands::ctx::config::EndpointTarget {
                    vendor: "zhipu".to_string(),
                    base_url: "https://api.z.ai/api/anthropic".to_string(),
                    credential_env: "ZIRV_TEST_ROSTER_ZHIPU_KEY_395".to_string(),
                    model: None,
                    wire_api: None,
                }),
                codex: None,
            },
            ..super::super::tests::permissive_cfg()
        };
        let lines = harness_prompt_lines(&cfg, "");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .unwrap_or_else(|| panic!("no claude line in {lines:?}"));
        assert!(
            claude_line.contains("(endpoint: zhipu)"),
            "got {claude_line}"
        );
        assert!(
            !claude_line.contains("api.z.ai") && !claude_line.contains("sekrit"),
            "the roster line must never carry the URL or the credential: {claude_line}"
        );
        // SAFETY (test): see the matching `set_var` above.
        unsafe {
            std::env::remove_var("ZIRV_TEST_ROSTER_ZHIPU_KEY_395");
        }
    }

    /// No enabled harness at all: `review_roster_line` must not emit a
    /// line naming zero harnesses -- absence, not an empty-handed line.
    /// Issue #298 widens the claim to the whole roster: with both adapters
    /// disabled (so their own per-adapter lines are omitted too, the same
    /// "absence, not annotation" rule as an absent binary), `harness_prompt_
    /// lines` returns a genuinely empty vec -- the precondition `prompt::
    /// compose`'s own `!harness_lines.is_empty()` gate relies on to skip the
    /// whole "zirv harness roster (session)" header rather than render one
    /// with nothing under it.
    #[test]
    fn harness_prompt_lines_returns_nothing_when_no_harness_is_enabled() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
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
        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            lines.is_empty(),
            "no harness enabled: the whole roster must be empty, not just the review line: \
             {lines:?}"
        );
    }

    /// The one call site (`prompt::compose` for an Orchestrator session) must
    /// never learn about a disabled adapter at all, whether as an annotated
    /// "disabled" line or as the `zirv agent <name>` invitation -- issue
    /// #298: absence, not annotation, the same rule an absent binary gets
    /// below.
    #[test]
    fn harness_prompt_lines_omits_the_disabled_adapter_entirely() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env: std::collections::HashMap<String, String> =
            [("ZIRV_AGENT_CODEX_ENABLED".to_string(), "false".to_string())]
                .into_iter()
                .collect();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| env.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            !lines.iter().any(|l| l.starts_with("- codex:")),
            "a disabled adapter must contribute no line at all: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("zirv agent codex")),
            "a disabled adapter is never offered for delegation: {lines:?}"
        );
    }

    /// Finding 1 (pre-#298): `ready()` alone is fail-open for a program that
    /// simply is not on disk anywhere -- `resolve_program` deliberately
    /// returns `Ok` for it (see its own doc comment), and several other call
    /// sites lean on that. Issue #298 changes what a resolved-but-absent
    /// program earns from "not installed" annotation to no line at all: a
    /// name that resolves to nothing must cost the session zero bytes, not
    /// spend them on a claim `program_is_present`'s own doc comment already
    /// admits can be wrong.
    #[test]
    fn harness_prompt_lines_omits_the_line_when_the_resolved_program_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nonexistent-agent-binary");
        let cfg = CtxConfig {
            agent_bin: Some(missing.display().to_string()),
            ..super::super::tests::permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            !lines.iter().any(|l| l.starts_with("- claude:")),
            "an absent binary must contribute no line at all: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("zirv agent")),
            "a binary that is not there must never be offered for delegation: {lines:?}"
        );
    }

    /// Bug A (harness/model parity), reframed for issue #298: two harnesses
    /// in the identical state -- here, both absent, behind the same missing
    /// `agent_bin` override -- must be treated identically. Pre-#298 that
    /// meant "the same annotated template"; now it means "both omitted".
    /// Review follow-up: with neither adapter earning its own roster line,
    /// the trailing review line must vanish too -- it would otherwise be the
    /// one line left directing a review to a harness the roster just said
    /// nothing about.
    #[test]
    fn harness_prompt_lines_omits_both_adapters_equally_in_the_same_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nonexistent-agent-binary");
        let cfg = CtxConfig {
            agent_bin: Some(missing.display().to_string()),
            ..super::super::tests::permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "");
        assert!(
            !lines.iter().any(|l| l.starts_with("- claude:")),
            "claude must be equally absent: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("- codex:")),
            "codex must be equally absent: {lines:?}"
        );
        assert!(
            lines.is_empty(),
            "with both adapters confirmed absent, the review line must not survive as the one \
             line still naming them: {lines:?}"
        );
    }

    /// Finding 1's positive case, plus Finding 4: a program that genuinely
    /// exists on disk is still offered for delegation, except when it is
    /// this session's own adapter, which is marked as such instead of
    /// inviting a session to delegate to itself.
    ///
    /// Each adapter's own default program name (no `agent_bin` override) is
    /// planted as a real stub on a PATH restricted to one temp dir, so
    /// codex's "ready" verdict here is earned by codex's own binary, never
    /// borrowed from an unrelated stub -- see the follow-up regression right
    /// below this test for the case where a *shared* override used to make
    /// that borrowing happen.
    #[test]
    fn harness_prompt_lines_offers_delegation_only_to_a_present_non_self_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["claude", "codex"] {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = super::super::tests::permissive_cfg();

        let lines = harness_prompt_lines(&cfg, "claude");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            claude_line.contains("this session's harness"),
            "got {claude_line}"
        );
        assert!(
            !claude_line.contains("zirv agent claude"),
            "a session never invites itself to delegate: {claude_line}"
        );

        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            codex_line.contains("zirv agent codex"),
            "a present, non-self adapter is still offered on the strength of its own binary: \
             {codex_line}"
        );
    }

    /// Item 1 regression: `harness_prompt_lines` used to build *every*
    /// adapter with the same global `agent_bin` override, so `agent_bin`
    /// naming claude's binary made codex's line borrow claude's presence
    /// verdict and falsely offer `zirv agent codex` -- a wasted delegation
    /// every review round, since `select` would go on to refuse it
    /// ("agent_bin names 'claude', not 'codex'"). With no `codex` binary
    /// anywhere on this test's restricted `PATH`, codex must read as not
    /// installed regardless of how present claude's own stub is.
    #[test]
    fn harness_prompt_lines_never_borrows_a_named_adapters_presence_for_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude"), "").expect("write stub");
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = CtxConfig {
            agent_bin: Some(dir.path().join("claude").display().to_string()),
            ..super::super::tests::permissive_cfg()
        };

        let lines = harness_prompt_lines(&cfg, "claude");
        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            claude_line.contains("this session's harness"),
            "claude's own named override is present: {claude_line}"
        );

        assert!(
            !lines.iter().any(|l| l.starts_with("- codex:")),
            "codex is judged on its own (absent) binary, not claude's override, so issue #298 \
             omits its line entirely rather than claiming 'not installed': {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("zirv agent codex")),
            "agent_bin naming claude must never make codex's line claim delegable: {lines:?}"
        );
    }

    // -- issue #298 ("capability-gated injection"): the Liveness predicate,
    // the widened install-root probe, and the per-repository ProbeCache --

    /// The success metric the issue's own probe cache exists for: a second
    /// `harness_prompt_lines_cached` call against the same cache must
    /// deliver byte-identical lines even after the underlying filesystem
    /// truth changes mid-session -- the injected prompt prefix stays stable
    /// across a session's turns.
    #[test]
    fn harness_prompt_lines_cached_survives_a_mid_session_filesystem_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in ADAPTERS {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let cfg = super::super::tests::permissive_cfg();
        let mut cache = ProbeCache {
            path: None,
            now: 1_000,
            file: ProbeCacheFile::default(),
            dirty: false,
        };

        let first = harness_prompt_lines_cached(&cfg, "", &mut cache).lines;
        for (name, _) in ADAPTERS {
            std::fs::remove_file(dir.path().join(name)).expect("remove stub");
        }
        let second = harness_prompt_lines_cached(&cfg, "", &mut cache).lines;
        assert_eq!(
            first, second,
            "a cached verdict must survive a mid-session change on disk, not flap the roster: \
             first={first:?} second={second:?}"
        );
    }

    /// A capacity-limited harness's roster line gets the `-- small tasks
    /// only` suffix; an unmarked harness's line does not. This is the
    /// signal `HARNESS_PROMPT`'s final paragraph tells an orchestrator to
    /// route only small, bounded briefs by, for both reviews and `zirv
    /// agent` delegations.
    #[test]
    fn harness_prompt_lines_marks_a_capacity_limited_harness_small_tasks_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["claude", "codex"] {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);

        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.codex]\ncapacity = \"small\"\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = CtxConfig {
            agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                .expect("load"),
            ..CtxConfig::default()
        };

        let lines = harness_prompt_lines(&cfg, "claude");
        let codex_line = lines
            .iter()
            .find(|l| l.starts_with("- codex:"))
            .expect("codex line present");
        assert!(
            codex_line.contains("ready -- small tasks only"),
            "got {codex_line}"
        );

        let claude_line = lines
            .iter()
            .find(|l| l.starts_with("- claude:"))
            .expect("claude line present");
        assert!(
            !claude_line.contains("small tasks only"),
            "claude was never marked capacity-small: {claude_line}"
        );
    }

    /// H1/H2: both `readiness_note`'s "not ready yet" clause and `resolve_
    /// default`'s `Err(e) => continue` unready-skip branch lost their only
    /// coverage once codex's own `ready()` stopped hard-erroring -- nothing
    /// in the real registry is ever actually unready anymore, so a test that
    /// only reads the real `ADAPTERS` table can no longer exercise either
    /// branch at all. This forces claude's own bare `"claude"` name to
    /// resolve to an unlaunchable `.py` (the same PATH/PATHEXT rig `an_
    /// unlaunchable_program_on_path_is_named_rather_than_left_to_error_193`
    /// uses), which is the one real way `ready()` fails on this codebase,
    /// leaving codex genuinely unaffected (codex.cmd, wherever it resolves
    /// or fails to, is never a `ready()` error case) to prove the skip-and-
    /// continue path lands on it.
    #[cfg(windows)]
    #[test]
    fn readiness_note_and_the_fallback_skip_both_stay_covered_when_an_adapter_is_genuinely_unready()
    {
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

        // H1: the "not ready yet" clause is genuinely exercised again.
        let note = readiness_note();
        assert!(
            note.to_lowercase().contains("not ready"),
            "claude must be reported not ready under this rig: {note}"
        );
        assert!(note.contains("claude"), "got {note}");

        // H2: `resolve_default`'s fallback must skip claude's `Err` and land
        // on codex, exercising the `Err(e) => reasons.push(...); continue`
        // arm rather than the `Ok(())` one.
        let (adapter, origin) = resolve_default_with_presence(
            &super::super::tests::permissive_cfg(),
            &everything_installed(),
        )
        .expect("codex still qualifies");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(origin, DefaultOrigin::FirstEnabledReady);
    }
}
