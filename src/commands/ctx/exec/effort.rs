//! Headless cost controls and persistent effort selection.

use super::*;

/// Apply operator-only Claude headless cost settings. Effort defaults to
/// `low` for every class and the cache TTL to `5m`; every other unset
/// setting, and other adapters, leave the launch unchanged. (#788)
pub(super) fn apply_headless_cost_levers(
    command: &mut Command,
    cfg: &CtxConfig,
    adapter_name: &str,
    prompt: Option<&str>,
    state: &StateDir,
    session: &SessionId,
) {
    if adapter_name != "claude" {
        return;
    }
    let headless = &cfg.headless;
    if let Some(ttl) = headless.prompt_cache_ttl.as_deref() {
        let operator_env_wins = [
            "CLAUDE_CODE_PROMPT_CACHE_TTL",
            "FORCE_PROMPT_CACHING_5M",
            "ENABLE_PROMPT_CACHING_1H",
        ]
        .iter()
        .any(|name| std::env::var(name).is_ok());
        if !operator_env_wins {
            command.env("CLAUDE_CODE_PROMPT_CACHE_TTL", ttl);
        }
    }
    if std::env::var("CLAUDE_CODE_EFFORT_LEVEL").is_ok() {
        return;
    }
    let argv_has_effort = command.get_args().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == "--effort" || arg.starts_with("--effort=")
    });
    if argv_has_effort {
        return;
    }
    if let Some(effort) = sticky_headless_effort(state, cfg, session, prompt) {
        command.env("CLAUDE_CODE_EFFORT_LEVEL", effort);
    }
}

/// Pin effort for a whole conversation: changing it on resume invalidates
/// Claude prompt caching. I/O failure falls back to this launch's classifier.
/// A gated Jev choice uses numeric facts only and shares the same pin. (#788)
fn sticky_headless_effort(
    state: &StateDir,
    cfg: &CtxConfig,
    session: &SessionId,
    prompt: Option<&str>,
) -> Option<String> {
    let headless = &cfg.headless;
    let record_path = headless_effort_record_path(state, session);
    if let Some(record) = load_headless_effort_record(&record_path) {
        return record.effort;
    }
    let classification = prompt.and_then(super::proxy::decision::try_classify_request);
    let deterministic = classification
        .as_ref()
        .and_then(|classification| headless_effort_for(&headless.effort, classification.complexity))
        .map(str::to_string)
        .or_else(|| {
            // No prompt or no classification: only a value shared by every
            // class is unambiguous (the shipped all-`low` default).
            let e = &headless.effort;
            (e.trivial == e.bounded && e.bounded == e.substantial)
                .then(|| e.trivial.clone())
                .flatten()
        });
    let effort = match (prompt, &classification) {
        (Some(prompt), Some(classification)) => {
            jev_launch_effort(state, cfg, prompt, classification.complexity).or(deterministic)
        }
        _ => deterministic,
    };
    save_headless_effort_record(
        state,
        &record_path,
        &HeadlessEffortRecord {
            effort: effort.clone(),
        },
    );
    effort
}

/// Send only bounded numeric metadata, never prompt text, to Jev.
#[derive(Debug, Serialize)]
struct LaunchEffortAdviseState {
    #[serde(rename = "_zirv_metadata_only")]
    metadata_only: bool,
    facts: Vec<Vec<u32>>,
}

pub(crate) fn launch_effort_question() -> [jev::Question; 1] {
    [jev::Question::metadata_noul(
        "launch_effort_high",
        "Facts [word-count bucket (0<10,1<50,2<200,3>=200), enumerated-item count, deterministic \
complexity index (0 trivial..3 architectural), reads as a question (1) or not (0)] describe one \
request at a headless launch's first turn. Is it unusually hard, deliberate work needing HIGH \
reasoning effort (not just long)? False for an ordinary/small follow-up. False if unsure.",
        "unusually hard, deliberate work warranting high effort",
        "a small, low-deliberation follow-up warranting low effort",
    )]
}

/// Derive local numeric facts without sending prompt text to Jev.
fn launch_effort_facts(prompt: &str, complexity: Complexity) -> Vec<u32> {
    let words = prompt.split_whitespace().count();
    let word_bucket: u32 = match words {
        0..10 => 0,
        10..50 => 1,
        50..200 => 2,
        _ => 3,
    };
    let items: u32 = prompt
        .lines()
        .map(str::trim_start)
        .filter(|line| {
            line.starts_with("- ")
                || line.starts_with("* ")
                || line.split_once(['.', ')']).is_some_and(|(n, rest)| {
                    (1..=2).contains(&n.len())
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && rest.starts_with(' ')
                })
        })
        .count()
        .min(20) as u32;
    let complexity_index: u32 = match complexity {
        Complexity::Trivial => 0,
        Complexity::Bounded => 1,
        Complexity::Substantial => 2,
        Complexity::Architectural => 3,
    };
    let looks_like_a_question = u32::from(prompt.trim_end().ends_with('?'));
    vec![word_bucket, items, complexity_index, looks_like_a_question]
}

pub(crate) const LAUNCH_EFFORT_DEFAULT_FLOOR: (f32, f32) = (0.0, jev::DEFAULT_MIN_MARGIN);

/// Only a decisive YES selects high. Anything else (a decisive no, an
/// indecisive or missing answer) uses the classifier: "not unusually hard" is
/// no evidence of a small follow-up, so Jev never lowers the pick.
pub(crate) fn launch_effort_action(
    answer: Option<&jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let decisive_yes = answer.is_some_and(|answer| {
        answer.decisive(min_confidence, min_margin)
            && answer.as_noul().is_some_and(|value| value >= 0.5)
    });
    if decisive_yes { "high" } else { "classifier" }
}

/// Jev may only RAISE the first launch to the configured `substantial` tier,
/// from numeric facts only; every other outcome falls back to classification. (#802)
fn jev_launch_effort(
    state: &StateDir,
    cfg: &CtxConfig,
    prompt: &str,
    complexity: Complexity,
) -> Option<String> {
    if !jev::gate_open(cfg, state, "launch_effort", cfg.jev.launch_effort) {
        return None;
    }
    let advise_state = LaunchEffortAdviseState {
        metadata_only: true,
        facts: vec![launch_effort_facts(prompt, complexity)],
    };
    let answers = jev::advise(
        cfg,
        state,
        "launch_effort",
        cfg.jev.launch_effort,
        &advise_state,
        &launch_effort_question(),
    )?;
    let answer = answers.get("launch_effort_high")?;
    let (min_confidence, min_margin) = jev::floor(
        cfg,
        jev::FloorSite::LaunchEffort,
        LAUNCH_EFFORT_DEFAULT_FLOOR.0,
        LAUNCH_EFFORT_DEFAULT_FLOOR.1,
    );
    if launch_effort_action(Some(answer), min_confidence, min_margin) != "high" {
        return None;
    }
    let chosen = cfg.headless.effort.substantial.clone()?;
    if headless_effort_for(&cfg.headless.effort, complexity) == Some(chosen.as_str()) {
        // Jev would not change the pick: the caller uses the identical
        // deterministic value, and no effect is recorded.
        return None;
    }
    let effect = jev::JevEffect::new("launch_effort", "effort_high");
    jev::record_effect(cfg, state, cfg.jev.launch_effort, &effect);
    Some(chosen)
}

/// Persist even a decision of no configured effort for this conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeadlessEffortRecord {
    effort: Option<String>,
}

/// Per-conversation effort record path.
fn headless_effort_record_path(state: &StateDir, session: &SessionId) -> PathBuf {
    state
        .headless_effort()
        .join(format!("{:016x}.json", input_hash(session.as_str())))
}

/// Missing or invalid state means no stored decision, without failing launch.
fn load_headless_effort_record(path: &Path) -> Option<HeadlessEffortRecord> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Failed persistence may cause reclassification later but cannot fail
/// this launch; prune old records after successful writes.
fn save_headless_effort_record(state: &StateDir, path: &Path, record: &HeadlessEffortRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = super::state::create_private_dir_all(dir);
    }
    if super::state::write_private(path, &json).is_ok() {
        super::state::prune_to_newest(&state.headless_effort(), super::state::KEEP_NEWEST);
    }
}

/// Resolve configured effort; text-only classification cannot produce an
/// architectural tier, so it shares substantial effort.
fn headless_effort_for(
    effort: &super::config::HeadlessEffortConfig,
    complexity: Complexity,
) -> Option<&str> {
    match complexity {
        Complexity::Trivial => effort.trivial.as_deref(),
        Complexity::Bounded => effort.bounded.as_deref(),
        Complexity::Substantial | Complexity::Architectural => effort.substantial.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launch-effort request must pass the egress guard, or the site goes quiet.
    #[test]
    fn the_launch_effort_request_passes_the_egress_guard() {
        let state = serde_json::to_value(LaunchEffortAdviseState {
            metadata_only: true,
            facts: vec![launch_effort_facts(
                "implement the export across several modules, then document it",
                Complexity::Substantial,
            )],
        })
        .expect("serializes");
        assert!(jev::safe_metadata_request(
            &state,
            &launch_effort_question(),
            "jev-latest"
        ));
    }

    /// With no operator config, a headless launch carries
    /// `CLAUDE_CODE_EFFORT_LEVEL=low` for every class (and no cache-TTL env);
    /// an explicit operator value for a class still wins over the default.
    #[test]
    fn apply_headless_cost_levers_defaults_effort_to_low_and_explicit_value_wins() {
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", None),
            ("FORCE_PROMPT_CACHING_5M", None),
            ("ENABLE_PROMPT_CACHING_1H", None),
            ("CLAUDE_CODE_EFFORT_LEVEL", None),
        ]);
        let cfg = CtxConfig::default();
        let low = Some("low".to_string());
        assert_eq!(
            (
                &cfg.headless.effort.trivial,
                &cfg.headless.effort.bounded,
                &cfg.headless.effort.substantial
            ),
            (&low, &low, &low)
        );
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let session = SessionId::new_v4();
        let mut command = Command::new("claude");
        command.arg("-p").arg("do a small thing");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some("do a small thing"),
            &state,
            &session,
        );
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs,
            vec![
                (
                    std::ffi::OsStr::new("CLAUDE_CODE_EFFORT_LEVEL"),
                    Some(std::ffi::OsStr::new("low"))
                ),
                (
                    std::ffi::OsStr::new("CLAUDE_CODE_PROMPT_CACHE_TTL"),
                    Some(std::ffi::OsStr::new("5m"))
                ),
            ],
            "an unconfigured [headless] table must add only effort=low and ttl=5m"
        );

        // No prompt text (nothing to classify): the shared default still applies.
        let mut bare = Command::new("claude");
        apply_headless_cost_levers(
            &mut bare,
            &cfg,
            "claude",
            None,
            &state,
            &SessionId::new_v4(),
        );
        assert_eq!(
            bare.get_envs().collect::<Vec<_>>(),
            vec![
                (
                    std::ffi::OsStr::new("CLAUDE_CODE_EFFORT_LEVEL"),
                    Some(std::ffi::OsStr::new("low"))
                ),
                (
                    std::ffi::OsStr::new("CLAUDE_CODE_PROMPT_CACHE_TTL"),
                    Some(std::ffi::OsStr::new("5m"))
                ),
            ]
        );

        // An explicit operator value wins over the default for its class.
        let mut explicit = CtxConfig::default();
        explicit.headless.effort.trivial = Some("medium".to_string());
        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &explicit,
            "claude",
            Some("do a small thing"),
            &state,
            &SessionId::new_v4(),
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("medium"))
        );
    }

    /// With no operator config or env, a headless launch carries
    /// `CLAUDE_CODE_PROMPT_CACHE_TTL=5m` (the shipped default); an operator
    /// `1h` still wins over it and the operator's own env still skips it.
    #[test]
    fn apply_headless_cost_levers_defaults_prompt_cache_ttl_to_5m() {
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", None),
            ("FORCE_PROMPT_CACHING_5M", None),
            ("ENABLE_PROMPT_CACHING_1H", None),
        ]);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let launch_ttl = |cfg: &CtxConfig| {
            let mut command = Command::new("claude");
            apply_headless_cost_levers(
                &mut command,
                cfg,
                "claude",
                None,
                &state,
                &SessionId::new_v4(),
            );
            command
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_PROMPT_CACHE_TTL")
                .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };

        let mut cfg = CtxConfig::default();
        assert_eq!(launch_ttl(&cfg).as_deref(), Some("5m"));
        cfg.headless.prompt_cache_ttl = Some("1h".to_string());
        assert_eq!(launch_ttl(&cfg).as_deref(), Some("1h"));

        let _operator_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ENABLE_PROMPT_CACHING_1H",
            Some("1"),
        )]);
        assert_eq!(launch_ttl(&CtxConfig::default()), None);
    }

    /// Issue #788: `headless.prompt_cache_ttl` sets `CLAUDE_CODE_PROMPT_
    /// CACHE_TTL` when configured, and the operator's own process env --
    /// `CLAUDE_CODE_PROMPT_CACHE_TTL` itself, or either alias the vendor
    /// docs name -- wins over it.
    #[test]
    fn apply_headless_cost_levers_sets_prompt_cache_ttl_and_operator_env_wins() {
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", None),
            ("FORCE_PROMPT_CACHING_5M", None),
            ("ENABLE_PROMPT_CACHING_1H", None),
        ]);
        let mut cfg = CtxConfig::default();
        cfg.headless.prompt_cache_ttl = Some("1h".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            None,
            &state,
            &SessionId::new_v4(),
        );
        let ttl = command
            .get_envs()
            .find(|(key, _)| *key == "CLAUDE_CODE_PROMPT_CACHE_TTL")
            .and_then(|(_, value)| value);
        assert_eq!(ttl, Some(std::ffi::OsStr::new("1h")));

        let _operator_env =
            crate::commands::ctx::testenv::VarGuard::set(&[("FORCE_PROMPT_CACHING_5M", Some("1"))]);
        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            None,
            &state,
            &SessionId::new_v4(),
        );
        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != "CLAUDE_CODE_PROMPT_CACHE_TTL"),
            "the operator's own FORCE_PROMPT_CACHING_5M must win over a configured ttl"
        );
    }

    /// Issue #788: `headless.effort.<class>` sets `CLAUDE_CODE_EFFORT_LEVEL`
    /// for a request that classifies into a CONFIGURED class, and adds
    /// nothing for one that classifies into an UNCONFIGURED class -- the
    /// deterministic size-floor classifier (`proxy::decision::
    /// try_classify_request`) is text-only, never a Jev call.
    #[test]
    fn apply_headless_cost_levers_sets_effort_for_a_configured_class_and_skips_an_unset_one() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let mut cfg = CtxConfig::default();
        cfg.headless.effort.trivial = Some("low".to_string());
        // `bounded` is deliberately cleared (the shipped default is `low`).
        cfg.headless.effort.bounded = None;
        cfg.headless.effort.substantial = None;
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &SessionId::new_v4(),
        );
        let effort = command
            .get_envs()
            .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
            .and_then(|(_, value)| value);
        assert_eq!(
            effort,
            Some(std::ffi::OsStr::new("low")),
            "a short, unenumerated request classifies Trivial"
        );

        // A different session id, so this is a fresh classification and not
        // the first call's record being replayed.
        let bounded_request = "word ".repeat(150);
        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some(&bounded_request),
            &state,
            &SessionId::new_v4(),
        );
        assert_eq!(
            command
                .get_envs()
                .filter(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .count(),
            0,
            "a 150-word request floors to Bounded, which has no configured effort"
        );
    }

    /// Issue #788 review finding L3: the text-only classifier
    /// (`try_classify_request`) can never produce `Complexity::
    /// Architectural`, so there is no separate `headless.effort.
    /// architectural` key -- `Architectural` reads the SAME `substantial`
    /// value instead of silently doing nothing.
    #[test]
    fn headless_effort_for_maps_architectural_to_the_substantial_value() {
        let effort = crate::commands::ctx::config::HeadlessEffortConfig {
            substantial: Some("high".to_string()),
            ..Default::default()
        };
        assert_eq!(
            headless_effort_for(&effort, Complexity::Architectural),
            Some("high")
        );
        assert_eq!(
            headless_effort_for(&effort, Complexity::Substantial),
            Some("high")
        );
    }

    /// Issue #788: the operator's own `CLAUDE_CODE_EFFORT_LEVEL` process env,
    /// and an operator argv that already carries `--effort`, each independently
    /// win over a configured `headless.effort.*` value.
    #[test]
    fn apply_headless_cost_levers_skips_effort_when_the_operator_env_or_argv_already_wins() {
        let mut cfg = CtxConfig::default();
        cfg.headless.effort.trivial = Some("low".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        {
            let _env = crate::commands::ctx::testenv::VarGuard::set(&[(
                "CLAUDE_CODE_EFFORT_LEVEL",
                Some("high"),
            )]);
            let mut command = Command::new("claude");
            apply_headless_cost_levers(
                &mut command,
                &cfg,
                "claude",
                Some("fix the typo"),
                &state,
                &SessionId::new_v4(),
            );
            assert_eq!(
                command
                    .get_envs()
                    .filter(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                    .count(),
                0,
                "the operator's own CLAUDE_CODE_EFFORT_LEVEL must win"
            );
        }

        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let mut command = Command::new("claude");
        command.arg("--effort").arg("max");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &SessionId::new_v4(),
        );
        assert_eq!(
            command
                .get_envs()
                .filter(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .count(),
            0,
            "an argv that already carries --effort must win"
        );
    }

    /// Issue #788 follow-up: a second launch of the SAME session, whose own
    /// prompt would classify to a DIFFERENT configured effort than the first
    /// launch's, keeps the first launch's decision -- a resume must never
    /// flip `CLAUDE_CODE_EFFORT_LEVEL` mid-conversation, since that
    /// invalidates the whole prompt cache.
    #[test]
    fn apply_headless_cost_levers_keeps_the_first_launchs_effort_across_a_differently_classified_resume()
     {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let mut cfg = CtxConfig::default();
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.bounded = Some("medium".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let session = SessionId::new_v4();

        let mut first = Command::new("claude");
        apply_headless_cost_levers(
            &mut first,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &session,
        );
        assert_eq!(
            first
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low")),
            "the first launch classifies Trivial"
        );

        // A resume of the SAME session, with a prompt that on its own would
        // classify Bounded.
        let bounded_request = "word ".repeat(150);
        let mut resumed = Command::new("claude");
        apply_headless_cost_levers(
            &mut resumed,
            &cfg,
            "claude",
            Some(&bounded_request),
            &state,
            &session,
        );
        assert_eq!(
            resumed
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low")),
            "a resume of the SAME session must replay the first launch's effort, not reclassify \
             its own differently-sized prompt"
        );
    }

    /// Issue #788 follow-up: a bare resume with no new prompt text
    /// (`prompt == None`) of an ALREADY-decided session must still get that
    /// session's recorded effort -- not silently skip the lever, which would
    /// itself be a flip (configured effort, then none).
    #[test]
    fn apply_headless_cost_levers_reuses_the_recorded_effort_when_a_resume_has_no_new_prompt() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let mut cfg = CtxConfig::default();
        cfg.headless.effort.trivial = Some("low".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let session = SessionId::new_v4();

        let mut first = Command::new("claude");
        apply_headless_cost_levers(
            &mut first,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &session,
        );
        assert_eq!(
            first
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low"))
        );

        let mut resumed = Command::new("claude");
        apply_headless_cost_levers(&mut resumed, &cfg, "claude", None, &state, &session);
        assert_eq!(
            resumed
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low")),
            "prompt == None on a resume of the SAME session must reuse the recorded effort, not \
             skip the lever"
        );
    }

    /// Issue #788 follow-up: a DIFFERENT session id has no recorded decision
    /// yet, so it classifies its own prompt fresh rather than inheriting
    /// another, unrelated session's record.
    #[test]
    fn apply_headless_cost_levers_classifies_fresh_for_a_different_session_id() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let mut cfg = CtxConfig::default();
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.bounded = Some("medium".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let session_a = SessionId::new_v4();
        let mut first = Command::new("claude");
        apply_headless_cost_levers(
            &mut first,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &session_a,
        );
        assert_eq!(
            first
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low"))
        );

        let session_b = SessionId::new_v4();
        let bounded_request = "word ".repeat(150);
        let mut second = Command::new("claude");
        apply_headless_cost_levers(
            &mut second,
            &cfg,
            "claude",
            Some(&bounded_request),
            &state,
            &session_b,
        );
        assert_eq!(
            second
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("medium")),
            "a DIFFERENT session id must classify its own prompt fresh, not inherit another \
             session's record"
        );
    }

    /// `[jev] launch_effort` test config: the same shape `hook.rs`'s own
    /// `stop_verify_cfg` uses for its Jev-gated tests, pointed at a caller-
    /// supplied base URL so a one-shot local server (or a deliberately
    /// unreachable port) stands in for the network.
    fn launch_effort_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.launch_effort = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    /// `[jev] launch_effort` gate off: byte-identical to today, even with a
    /// reachable Jev endpoint and a fully configured `[headless.effort]` --
    /// `sticky_headless_effort` never reaches `jev_launch_effort` at all, so
    /// no `jev-decisions.jsonl` row is ever written.
    #[test]
    fn apply_headless_cost_levers_jev_launch_effort_gate_off_is_byte_identical() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let env = "EXEC_TEST_LAUNCH_EFFORT_OFF";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        // Port 9 ("discard") refuses every connection -- if the gate being
        // off did not actually skip the call, this would time out or error,
        // never silently succeed.
        let mut cfg = launch_effort_cfg("http://127.0.0.1:9".to_string(), env);
        cfg.jev.launch_effort = false;
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.substantial = Some("high".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &SessionId::new_v4(),
        );
        unsafe { std::env::remove_var(env) };
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low")),
            "gate off must fall back to the plain deterministic classification"
        );
        assert!(
            !state.root().join("jev-decisions.jsonl").exists(),
            "gate off must never even attempt the call"
        );
    }

    /// `[jev] launch_effort` gate on with a decisive HIGH answer: overrides
    /// the deterministic pick (a short, unenumerated request classifies
    /// Trivial -> `headless.effort.trivial`) with `headless.effort.
    /// substantial` instead. A resumed launch of the SAME session, with no
    /// server listening at all, still replays that exact recorded value --
    /// the Jev choice goes through the identical sticky record the
    /// deterministic path always used.
    #[test]
    fn apply_headless_cost_levers_jev_launch_effort_decisive_high_overrides_and_stays_sticky() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let body = r#"{"model": "jev-latest", "answers": {
            "launch_effort_high": {"type": "noul", "noul": 0.93}},
            "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let env = "EXEC_TEST_LAUNCH_EFFORT_HIGH";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let mut cfg = launch_effort_cfg(url, env);
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.substantial = Some("high".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let session = SessionId::new_v4();

        let mut first = Command::new("claude");
        apply_headless_cost_levers(
            &mut first,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &session,
        );
        handle.join().expect("server thread");
        unsafe { std::env::remove_var(env) };
        assert_eq!(
            first
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("high")),
            "a decisive high answer must override the Trivial classification's own low pick"
        );

        // The resume: no server bound at all (a second request would hang
        // forever waiting for a connection that never comes), proving the
        // sticky record -- not a second Jev call -- is what answers this.
        let mut resumed = Command::new("claude");
        apply_headless_cost_levers(
            &mut resumed,
            &cfg,
            "claude",
            Some("do something else entirely"),
            &state,
            &session,
        );
        assert_eq!(
            resumed
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("high")),
            "a resume of the SAME session must replay the recorded Jev choice, never re-ask"
        );
    }

    /// `[jev] launch_effort` gate on with an indecisive answer (margin below
    /// `DEFAULT_MIN_MARGIN`): falls back to the plain deterministic
    /// classification, exactly as an unavailable or failed call would.
    #[test]
    fn apply_headless_cost_levers_jev_launch_effort_indecisive_falls_back_to_deterministic() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let body = r#"{"model": "jev-latest", "answers": {
            "launch_effort_high": {"type": "noul", "noul": 0.55}},
            "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let env = "EXEC_TEST_LAUNCH_EFFORT_INDECISIVE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let mut cfg = launch_effort_cfg(url, env);
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.substantial = Some("high".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some("fix the typo"),
            &state,
            &SessionId::new_v4(),
        );
        handle.join().expect("server thread");
        unsafe { std::env::remove_var(env) };
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("low")),
            "an indecisive answer must fall back to the deterministic Trivial -> low pick"
        );
    }

    /// A decisive NO never lowers effort: a Substantial classification (8+
    /// list items) keeps the `substantial` tier, not `trivial`, and no
    /// `launch_effort` effect is recorded because Jev changed nothing.
    #[test]
    fn apply_headless_cost_levers_jev_launch_effort_decisive_no_never_lowers() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let body = r#"{"model": "jev-latest", "answers": {
            "launch_effort_high": {"type": "noul", "noul": 0.02}},
            "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let env = "EXEC_TEST_LAUNCH_EFFORT_NO";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let mut cfg = launch_effort_cfg(url, env);
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.substantial = Some("high".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let prompt = (1..=8)
            .map(|n| format!("- change module number {n}\n"))
            .collect::<String>();

        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some(&prompt),
            &state,
            &SessionId::new_v4(),
        );
        handle.join().expect("server thread");
        unsafe { std::env::remove_var(env) };
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("high")),
            "a decisive no must keep the deterministic Substantial -> high pick, never lower it"
        );
        assert!(
            !state.root().join("jev-effects.jsonl").exists(),
            "Jev did not change the pick, so no effect is recorded"
        );
    }

    /// A decisive YES on a prompt that already classifies Substantial changes
    /// nothing: the substantial tier is used and no effect is recorded.
    #[test]
    fn apply_headless_cost_levers_jev_launch_effort_yes_on_substantial_records_no_effect() {
        let _env =
            crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CODE_EFFORT_LEVEL", None)]);
        let body = r#"{"model": "jev-latest", "answers": {
            "launch_effort_high": {"type": "noul", "noul": 0.97}},
            "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let env = "EXEC_TEST_LAUNCH_EFFORT_YES_SUBSTANTIAL";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let mut cfg = launch_effort_cfg(url, env);
        cfg.headless.effort.trivial = Some("low".to_string());
        cfg.headless.effort.substantial = Some("high".to_string());
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let prompt = (1..=8)
            .map(|n| format!("- change module number {n}\n"))
            .collect::<String>();

        let mut command = Command::new("claude");
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            "claude",
            Some(&prompt),
            &state,
            &SessionId::new_v4(),
        );
        handle.join().expect("server thread");
        unsafe { std::env::remove_var(env) };
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("high")),
            "a decisive yes on a Substantial prompt yields the substantial tier"
        );
        assert!(
            !state.root().join("jev-effects.jsonl").exists(),
            "Jev did not change the pick, so no effect is recorded"
        );
    }
}
