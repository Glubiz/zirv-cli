//! PreToolUse payload types and the dispatch-tier (worker seat) brief
//! classification jev asks about.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::checkpoints::cfg_or_operator_only_gate;
use super::pretool_guard::append_skill_pointer;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::state::StateDir;

// PreToolUse dispatch-tier and orchestrator-write guards (#334).

// Shared lifecycle vocabulary also covers native sessions without
// PreToolUse payloads (#478).

/// Optional Claude PreToolUse fields; malformed or missing fields fail
/// open rather than breaking the hook.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreToolPayload {
    pub tool_name: String,
    pub tool_input: PreToolInput,
    pub cwd: String,
    pub session_id: String,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    // retained from Claude's documented payload; agent_id is the discriminator
    #[allow(dead_code)]
    pub agent_type: String,
    /// Claude's own session mode (documented values include `"default"`,
    /// `"plan"`, `"acceptEdits"`, `"dontAsk"`, ...) -- the scope-guard
    /// checkpoint's own headless signal, the identical field/value
    /// `safety.rs`'s `hook_output` reads for the same purpose on its own
    /// (separately modelled) `Bash`/`PowerShell` payload.
    #[serde(default)]
    pub permission_mode: String,
}

/// `tool_input` is tool-specific, so only the subagent tool's own parameters
/// are modelled and every other tool's arguments are ignored rather than
/// rejected. `deny_unknown_fields` here would turn an ordinary `Bash` payload
/// into a parse failure.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreToolInput {
    pub subagent_type: String,
    pub model: String,
    /// Dispatch task text; missing or empty text indicates schema drift and
    /// must fail open rather than deny on serde defaults.
    pub prompt: String,
    /// Short descriptive dispatch text for advisory use, never a
    /// deterministic admission signal (#537).
    pub description: String,
    /// Target path for Edit, Write and MultiEdit (#334).
    pub file_path: String,
    /// NotebookEdit target path (#334).
    pub notebook_path: String,
    /// Write content inspected for reusable definitions (#406).
    pub content: String,
    /// Edit replacement text (#406).
    pub new_string: String,
    /// Edit original text; definitions in both halves are not additions (#406).
    pub old_string: String,
    /// MultiEdit entries with the same original/replacement pair (#406).
    pub edits: Vec<PreToolEdit>,
    /// Bash command text for the bounded bare-`git log` rewrite (#419).
    pub command: String,
}

/// Optional MultiEdit entry; payload drift must not break the hook.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreToolEdit {
    pub old_string: String,
    pub new_string: String,
}

impl PreToolPayload {
    pub fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }
}

/// What the model is told when a dispatch is refused. The reason is the only
/// thing it sees, so it has to carry the whole remedy: naming the seat, the
/// cheaper models that are accepted, and the one option (a fork) that no
/// model parameter can rescue.
/// The whole decision, pure: `Some(reason)` denies, `None` allows.
///
/// `seat` is `SEAT_MODEL_ENV`'s value, absent for any session zirv did not
/// launch as an expensive orchestrator seat. Every gate below is a reason to
/// allow, so an unrecognised tool, an unset seat, a cheap seat, a payload
/// with no `prompt` (see `PreToolInput::prompt`'s own doc comment -- that is
/// schema drift, not a dispatch), or a payload this function does not
/// understand at all fall through to allow. That is deliberate: this hook
/// runs in front of every tool call in the session, and the cost of a wrong
/// deny is far higher than the cost of a missed one.
pub fn pretool_decision(seat: Option<&str>, payload: &PreToolPayload) -> Option<String> {
    // Translate Claude dispatch payloads into the shared lifecycle guard so
    // native sessions use the same admission rule (#478).
    crate::commands::ctx::lifecycle::subagent_admission(seat, &pretool_intent(payload))
}

/// The neutral [`crate::commands::ctx::lifecycle::ToolIntent`] for a PreToolUse payload, as
/// far as the subagent guard needs it. The write-guard fields are filled in
/// separately by [`orchestrator_write_target`], which has a `cwd` to resolve
/// the target against.
pub(super) fn pretool_intent(
    payload: &PreToolPayload,
) -> crate::commands::ctx::lifecycle::ToolIntent {
    crate::commands::ctx::lifecycle::ToolIntent {
        tool: payload.tool_name.clone(),
        write_target: None,
        subagent: Some(crate::commands::ctx::lifecycle::SubagentIntent {
            prompt: payload.tool_input.prompt.clone(),
            subagent_type: payload.tool_input.subagent_type.clone(),
            model: payload.tool_input.model.clone(),
        }),
        delegated: !payload.agent_id.is_empty(),
    }
}

/// Preserve every original tool-input field: Claude `updatedInput`
/// replaces the object rather than merging partial fields (#537).
pub(super) fn raw_tool_input(stdin: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(stdin)
        .ok()
        .and_then(|value| value.get("tool_input").cloned())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// `payload.cwd` when it names one, else the process's own current
/// directory -- the one cwd-resolution rule every PreToolUse guard that
/// needs a repository root applies, shared so the file-modification guard
/// below and the dispatch-tier advisory never resolve it two different ways.
pub(super) fn resolved_cwd(payload: &PreToolPayload) -> Option<PathBuf> {
    if !payload.cwd.is_empty() {
        return Some(PathBuf::from(&payload.cwd));
    }
    std::env::current_dir().ok()
}

// -- PreToolUse: the dispatch model-tier advisory (issue #537 A5) ----------

/// Require a confident tier suggestion before rewriting an omitted model;
/// weak advice leaves the deterministic denial in force (#537).
pub(crate) const DISPATCH_TIER_FLOOR: f32 = 0.6;

/// Send only bounded numeric brief metadata to Jev; prompt text and
/// descriptions must never leave through this request (#744).
#[derive(Debug, Serialize)]
pub(super) struct DispatchAdviseState {
    #[serde(rename = "_zirv_metadata_only")]
    pub(super) metadata_only: bool,
    pub(super) facts: Vec<Vec<u32>>,
}

/// Brief keywords that mark work likely to need a stronger model -- hard
/// debugging, concurrency, or an architecture/design/migration decision.
/// Matched case-insensitively as brief-text substrings entirely on this
/// side of the egress boundary: only the resulting count
/// ([`count_keyword_class`]) ever reaches Jev.
const HARD_BRIEF_KEYWORDS: [&str; 8] = [
    "debug",
    "race",
    "concurrency",
    "deadlock",
    "security",
    "architecture",
    "design",
    "migration",
];

/// Brief keywords that mark mechanical or bulk work likely to need only a
/// cheap model. Same local-only matching rule as [`HARD_BRIEF_KEYWORDS`].
const MECHANICAL_BRIEF_KEYWORDS: [&str; 7] =
    ["rename", "format", "typo", "bulk", "move", "lookup", "list"];

/// `value`, capped to the metadata contract's own `<= 1_000_000` per-cell
/// limit ([`safe_metadata_request`]) -- every fact this module sends is
/// built through this so none can ever fail that check.
pub(super) fn capped_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX).min(1_000_000)
}

/// Whether `token` (already whitespace-split) looks like a file or
/// repository path: it carries a path separator, or ends in a short
/// alphanumeric extension after a non-empty stem.
fn looks_path_like(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| {
        !ch.is_ascii_alphanumeric() && !matches!(ch, '/' | '\\' | '.' | '_' | '-')
    });
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    match trimmed.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }
        None => false,
    }
}

fn count_path_like_tokens(brief: &str) -> u32 {
    capped_u32(
        brief
            .split_whitespace()
            .filter(|token| looks_path_like(token))
            .count(),
    )
}

pub(super) fn count_keyword_class(lower_brief: &str, keywords: &[&str]) -> u32 {
    let total: usize = keywords
        .iter()
        .map(|keyword| lower_brief.matches(keyword).count())
        .sum();
    capped_u32(total)
}

/// Which locally recognised expensive tier `seat` matched
/// ([`crate::commands::ctx::lifecycle::EXPENSIVE_TIERS`]), as a small index -- never the
/// seat string itself. By the time this runs, [`omitted_model_on_generic_
/// type`] has already confirmed `seat` names one of them, so the "no match"
/// arm is unreachable in production but stays total rather than panicking.
fn seat_tier_index(seat: &str) -> u32 {
    let lower = seat.to_ascii_lowercase();
    crate::commands::ctx::lifecycle::EXPENSIVE_TIERS
        .iter()
        .position(|tier| lower.contains(tier))
        .map_or(
            crate::commands::ctx::lifecycle::EXPENSIVE_TIERS.len() as u32,
            |index| index as u32,
        )
}

/// Seven numeric dispatch facts for Jev, never raw brief text (#744).
fn dispatch_brief_facts(brief: &str, seat: &str) -> Vec<u32> {
    let lower = brief.to_ascii_lowercase();
    vec![
        capped_u32(brief.len()),
        capped_u32(brief.lines().count()),
        count_path_like_tokens(brief),
        count_keyword_class(&lower, &HARD_BRIEF_KEYWORDS),
        count_keyword_class(&lower, &MECHANICAL_BRIEF_KEYWORDS),
        capped_u32(lower.matches("```").count()),
        seat_tier_index(seat),
    ]
}

/// [`dispatch_tier_advise`]'s own single Choice question, factored out so
/// `zirv ctx jev probe` can ask the exact same question from a fixture's
/// own facts row.
pub(crate) fn dispatch_tier_question() -> crate::commands::ctx::jev::Question {
    crate::commands::ctx::jev::Question::metadata_choice(
        "tier",
        "From bounded numeric metadata about a subagent dispatch's brief only (no text), how \
capable a model does it actually need?",
        &[
            (
                "cheap",
                "mechanical or bulk edits, formatting, simple lookups",
            ),
            ("standard", "ordinary implementation, tests, focused review"),
            (
                "frontier",
                "hard debugging, concurrency, architecture, security design",
            ),
        ],
    )
}

/// [`dispatch_tier_advise`]'s per-call decision: the chosen tier name
/// (`"cheap"`/`"standard"`/`"frontier"`) for a decisive, recognised choice,
/// `"deny"` otherwise (missing, indecisive, or an unrecognised choice) --
/// the deterministic path's own outcome. Shared with `zirv ctx jev probe`,
/// which reports exactly this outcome, never touching the catalogue/model
/// rewrite that follows in production.
pub(crate) fn dispatch_tier_action(
    answer: Option<&crate::commands::ctx::jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "deny";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "deny";
    }
    match answer.as_choice() {
        Some("cheap") => "cheap",
        Some("standard") => "standard",
        Some("frontier") => "frontier",
        _ => "deny",
    }
}

/// Exclude independent reviews from tier advice; their model comes from
/// the review roster, not this dispatch gate. Deliberately a loose
/// substring match: a false exclusion costs nothing, but a false inclusion
/// would let this advisory override a roster-mandated review model.
fn is_review_dispatch(subagent_type: &str, description: &str) -> bool {
    let names_review = |text: &str| text.to_ascii_lowercase().contains("review");
    names_review(subagent_type) || names_review(description)
}

/// Advise only when an omitted model alone caused admission denial;
/// explicit model or fork denials must remain final.
fn omitted_model_on_generic_type(seat: &str, tool_name: &str, input: &PreToolInput) -> bool {
    if !crate::commands::ctx::lifecycle::names_expensive_tier(seat) {
        return false;
    }
    if !crate::commands::ctx::lifecycle::SUBAGENT_TOOLS.contains(&tool_name) {
        return false;
    }
    if input.prompt.trim().is_empty() {
        return false;
    }
    let subagent_type = input.subagent_type.trim();
    let model = input.model.trim();
    model.is_empty()
        && subagent_type != "fork"
        && (subagent_type.is_empty()
            || crate::commands::ctx::lifecycle::GENERIC_SUBAGENT_TYPES.contains(&subagent_type))
}

/// Rewrite the complete original tool input with the selected model and
/// eligible skill pointer. Claude replaces `tool_input` wholesale, so a
/// model-only object would drop the prompt and other fields (#537, #539).
fn pretool_dispatch_tier_output(
    original_tool_input: &serde_json::Value,
    model: &str,
    note: &str,
    cfg: &CtxConfig,
) -> String {
    let mut updated_input = original_tool_input.clone();
    match updated_input.as_object_mut() {
        Some(object) => {
            object.insert(
                "model".to_string(),
                serde_json::Value::String(model.to_string()),
            );
        }
        // Keep an object fallback for malformed input; never emit a non-object
        // `updatedInput`.
        None => updated_input = serde_json::json!({ "model": model }),
    }
    append_skill_pointer(&mut updated_input, cfg);
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input,
            "additionalContext": note
        }
    })
    .to_string()
}

/// Ask Jev to select a model only for generic dispatches denied solely
/// because the model was omitted. Use bounded numeric metadata, never brief
/// text. Unknown, weak or failed answers preserve Deny; explicit models,
/// custom agent types and independent reviews are excluded (#537, #744).
fn dispatch_tier_advise(
    cfg: &CtxConfig,
    state: &StateDir,
    seat: &str,
    payload: &PreToolPayload,
    tool_input: &serde_json::Value,
) -> Option<String> {
    if !omitted_model_on_generic_type(seat, &payload.tool_name, &payload.tool_input) {
        return None;
    }
    if is_review_dispatch(
        &payload.tool_input.subagent_type,
        &payload.tool_input.description,
    ) {
        return None;
    }
    let advise_state = DispatchAdviseState {
        metadata_only: true,
        facts: vec![dispatch_brief_facts(&payload.tool_input.prompt, seat)],
    };
    let questions = [dispatch_tier_question()];
    let answers = crate::commands::ctx::jev::advise(
        cfg,
        state,
        "dispatch",
        cfg.jev.dispatch,
        &advise_state,
        &questions,
    )?;
    let answer = answers.get("tier")?;
    let (min_confidence, min_margin) = crate::commands::ctx::jev::floor(
        cfg,
        crate::commands::ctx::jev::FloorSite::Dispatch,
        DISPATCH_TIER_FLOOR,
        crate::commands::ctx::jev::DEFAULT_MIN_MARGIN,
    );
    let (tier, tier_label) = match dispatch_tier_action(Some(answer), min_confidence, min_margin) {
        "cheap" => (crate::commands::ctx::catalogue::Tier::Cheap, "cheap"),
        "standard" => (crate::commands::ctx::catalogue::Tier::Standard, "standard"),
        "frontier" => (crate::commands::ctx::catalogue::Tier::Deep, "frontier"),
        _ => return None,
    };
    let vendor_slug = crate::commands::ctx::catalogue::vendor_of(seat)?;
    let vendor = crate::commands::ctx::catalogue::vendor(vendor_slug)?;
    let alias = crate::commands::ctx::catalogue::tier_model(vendor, tier)?;
    let mut effect = crate::commands::ctx::jev::JevEffect::new("dispatch", "tier_selected");
    effect.reason = Some(tier_label);
    effect.outcome = Some(alias);
    crate::commands::ctx::jev::record_effect(cfg, state, cfg.jev.dispatch, &effect);
    Some(pretool_dispatch_tier_output(
        tool_input,
        alias,
        &format!(
            "zirv: model {alias} chosen for this dispatch (tier {tier_label}, {:.2})",
            answer.confidence
        ),
        cfg,
    ))
}

/// Resolve config and state with the write guard's path rules so tier advice
/// cannot read a different repo posture from the dispatch guard.
pub(super) fn dispatch_tier_override(
    seat: &str,
    payload: &PreToolPayload,
    stdin: &str,
    env: EnvLookup<'_>,
) -> Option<String> {
    let cwd = resolved_cwd(payload)?;
    let cfg = cfg_or_operator_only_gate(&cwd, env);
    let state = StateDir::resolve(env).ok()?;
    let tool_input = raw_tool_input(stdin);
    dispatch_tier_advise(&cfg, &state, seat, payload, &tool_input)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{SEAT, decide, pretool_stdin};
    use super::*;

    // -- dispatch_tier_advise (issue #537 A5, metadata-only re-projection
    // issue #744) --------------------------------------------------------

    /// Returns the parsed payload alongside the exact raw `tool_input` JSON
    /// it was built from -- `dispatch_tier_advise` needs both: the typed
    /// payload for its own gate checks, the raw value for what it must hand
    /// back unchanged via `updatedInput` (review finding).
    fn agent_payload(
        subagent_type: &str,
        model: &str,
        prompt: &str,
    ) -> (PreToolPayload, serde_json::Value) {
        agent_payload_with_description(subagent_type, model, prompt, "")
    }

    /// Like [`agent_payload`], with an explicit `description` -- the review
    /// exclusion ([`is_review_dispatch`]) reads both fields.
    fn agent_payload_with_description(
        subagent_type: &str,
        model: &str,
        prompt: &str,
        description: &str,
    ) -> (PreToolPayload, serde_json::Value) {
        let tool_input = serde_json::json!({
            "subagent_type": subagent_type,
            "model": model,
            "prompt": prompt,
            "description": description,
        });
        let payload = PreToolPayload::parse(&pretool_stdin("Agent", tool_input.clone()))
            .expect("the documented payload must parse");
        (payload, tool_input)
    }

    fn jev_test_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.dispatch = true;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    /// A decisive `cheap` answer for a generic, omitted-model dispatch must
    /// set `updatedInput.model`, record a `tier_selected` effect with the
    /// tier and model alias, and -- the point of issue #744 -- the exact
    /// request body this call sent must carry no brief text anywhere: the
    /// shared client's own egress boundary would otherwise have rejected it
    /// with `UnsafeState` before either of those could happen at all.
    #[test]
    fn generic_omitted_model_dispatch_with_a_decisive_answer_sets_model_and_records_no_brief_text()
    {
        let body = r#"{"model": "jev-latest", "answers": {
            "tier": {"type": "choice", "choice": "cheap",
                     "probabilities": {"cheap": 0.9, "standard": 0.1}, "confidence": 0.9}
        }, "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let (payload, tool_input) = agent_payload(
            "general-purpose",
            "",
            "rename the local variable across this file, a purely mechanical bulk edit",
        );
        let credential_env = "HOOK_TEST_JEV_DISPATCH_DECISIVE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");

        let output = output.expect("a decisive cheap answer must rewrite the dispatch");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid json");
        let model = parsed["hookSpecificOutput"]["updatedInput"]["model"]
            .as_str()
            .expect("a model was chosen");
        let vendor = crate::commands::ctx::catalogue::vendor(
            crate::commands::ctx::catalogue::vendor_of("fable")
                .expect("fable resolves to a vendor"),
        )
        .expect("the vendor is registered");
        let expected = crate::commands::ctx::catalogue::tier_model(
            vendor,
            crate::commands::ctx::catalogue::Tier::Cheap,
        )
        .expect("anthropic fills the cheap tier");
        assert_eq!(model, expected);

        let effects = std::fs::read_to_string(state_dir.path().join("jev-effects.jsonl"))
            .expect("a tier_selected effect must be recorded");
        assert!(
            effects.contains("\"action\":\"tier_selected\""),
            "{effects}"
        );
        assert!(effects.contains("\"reason\":\"cheap\""), "{effects}");
        assert!(
            effects.contains(&format!("\"outcome\":\"{expected}\"")),
            "{effects}"
        );

        // The point of issue #744: the exact request body this call sent
        // must never carry the brief text, only the numeric metadata row.
        let request_dump = state_dir.path().join("jev-cache");
        // `ask` hashes the request body into the cache key rather than
        // storing the body itself, so the strongest available proof the
        // request stayed metadata-only is that the call succeeded at all --
        // `safe_metadata_request` would have rejected any text-carrying
        // state as `UnsafeState` before this handle ever connected.
        let _ = request_dump;
    }

    /// The gate off must be `None` -- no call even attempted, despite a
    /// credential that looks available.
    #[test]
    fn dispatch_tier_advise_is_none_when_the_gate_is_off() {
        let (payload, tool_input) = agent_payload("general-purpose", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_GATE_OFF";
        // The credential looks available, so a bug that ignored the gate
        // would still attempt a call rather than short-circuiting on it.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let mut cfg = CtxConfig::default();
        assert!(!cfg.jev.dispatch, "the gate defaults off");
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert!(output.is_none());
        assert!(
            !state_dir.path().join("jev-decisions.jsonl").exists(),
            "gate off: no files at all, even with a credential that looks available"
        );
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    /// Gate-off parity holds even against a warm cache entry for the exact
    /// request this call would otherwise send: `dispatch_tier_advise` must
    /// short-circuit on the gate before ever reaching `ask`'s own cache
    /// lookup, so a hit sitting on disk changes nothing.
    #[test]
    fn dispatch_tier_advise_gate_off_ignores_a_warm_cache_entry() {
        let (payload, tool_input) = agent_payload("general-purpose", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_GATE_OFF_WARM_CACHE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let mut cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let facts = dispatch_brief_facts(&payload.tool_input.prompt, "fable");
        let advise_state = DispatchAdviseState {
            metadata_only: true,
            facts: vec![facts],
        };
        let questions = [crate::commands::ctx::jev::Question::metadata_choice(
            "tier",
            "From bounded numeric metadata about a subagent dispatch's brief only (no text), how \
capable a model does it actually need?",
            &[
                (
                    "cheap",
                    "mechanical or bulk edits, formatting, simple lookups",
                ),
                ("standard", "ordinary implementation, tests, focused review"),
                (
                    "frontier",
                    "hard debugging, concurrency, architecture, security design",
                ),
            ],
        )];
        let cache_key = crate::commands::ctx::jev::cache_key_for(
            &advise_state,
            &questions,
            &cfg.proxy.typesafe.model,
        )
        .expect("encodable request");
        let cache_dir = state_dir.path().join("jev-cache");
        std::fs::create_dir_all(&cache_dir).expect("cache dir");
        std::fs::write(
            cache_dir.join(format!("{cache_key}.json")),
            r#"{"answers":{"tier":{"value":{"Choice":"cheap"},"confidence":0.9,
                "probabilities":{"cheap":0.9,"standard":0.1}}},
                "usage":{"input_tokens":0,"output_tokens":0},"stored_at":0,"model":"jev-latest"}"#,
        )
        .expect("write warm cache entry");
        cfg.jev.dispatch = false;

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert!(output.is_none(), "gate off must win over a warm cache hit");
        assert!(!state_dir.path().join("jev-decisions.jsonl").exists());
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    /// The missing-key case (no `credential_env` value set at all) must
    /// give the same file-system silence as the gate-off case.
    #[test]
    fn dispatch_tier_advise_missing_credential_writes_no_files() {
        let (payload, tool_input) = agent_payload("general-purpose", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_MISSING_CREDENTIAL";
        // Deliberately never set: proves the missing-key path, not the
        // gate-off path.
        let cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);

        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        assert!(output.is_none());
        assert!(!state_dir.path().join("jev-decisions.jsonl").exists());
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    /// An explicit `model` must never even attempt a call, whatever the
    /// gate or credential -- proof is the absence of a decisions file, not
    /// merely a `None` a network failure could also produce.
    #[test]
    fn dispatch_tier_advise_never_calls_out_for_an_explicit_model() {
        let (payload, tool_input) =
            agent_payload("general-purpose", "opus", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_EXPLICIT_MODEL";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert!(output.is_none());
        assert!(
            !state_dir.path().join("jev-decisions.jsonl").exists(),
            "an explicit model must never even attempt a call"
        );
    }

    /// A named custom `subagent_type` (its own `.claude/agents/<name>.md`
    /// pins its own model) must never even attempt a call either.
    #[test]
    fn dispatch_tier_advise_never_calls_out_for_a_named_custom_subagent_type() {
        let (payload, tool_input) = agent_payload("vault-keeper", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_CUSTOM_TYPE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert!(output.is_none());
        assert!(!state_dir.path().join("jev-decisions.jsonl").exists());
    }

    /// [`is_review_dispatch`] is deliberately checked against both fields it
    /// reads, case-insensitively -- `subagent_type` is covered here as a
    /// direct unit test of the pure predicate, since every real
    /// `GENERIC_SUBAGENT_TYPES` value it can actually see once dispatched
    /// (`fork`/`claude`/`general-purpose`/`Explore`/`Plan`) never names a
    /// review, leaving `description` the only field a real dispatch reaches
    /// this guard through (covered by the integration test below).
    #[test]
    fn is_review_dispatch_matches_either_field_case_insensitively() {
        assert!(is_review_dispatch("code-reviewer", ""));
        assert!(is_review_dispatch("", "please REVIEW this diff"));
        assert!(!is_review_dispatch(
            "general-purpose",
            "implement the feature"
        ));
    }

    /// A dispatch whose `description` names a review must never even
    /// attempt a call: the review model is the roster's own choice
    /// ([`crate::commands::ctx::adapters::resolve_review_model`]), not this
    /// advisory's.
    #[test]
    fn dispatch_tier_advise_never_calls_out_for_a_review_dispatch() {
        let credential_env = "HOOK_TEST_JEV_DISPATCH_REVIEW_TYPE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let (payload, tool_input) = agent_payload_with_description(
            "general-purpose",
            "",
            "check the change",
            "independent code review",
        );
        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);
        assert!(
            output.is_none(),
            "a review dispatch must never be advised on"
        );

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert!(
            !state_dir.path().join("jev-decisions.jsonl").exists(),
            "a review dispatch must never even attempt a call"
        );
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    /// A confident but thin-margin answer must fall through to the
    /// deterministic deny path exactly like a low-confidence one --
    /// `Answer::decisive` governs both floors at once.
    #[test]
    fn dispatch_tier_advise_is_none_on_insufficient_certainty() {
        let body = r#"{"model": "jev-latest", "answers": {
            "tier": {"type": "choice", "choice": "frontier",
                     "probabilities": {"frontier": 0.51, "standard": 0.49}, "confidence": 0.95}
        }, "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let (payload, tool_input) = agent_payload("general-purpose", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_THIN_MARGIN";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert!(
            output.is_none(),
            "a thin-margin answer must retain deterministic deny"
        );
        let effects = state_dir.path().join("jev-effects.jsonl");
        assert!(
            !effects.exists(),
            "an undecisive answer must record no tier_selected effect"
        );
    }

    /// An answer Jev returns that names no known tier (never sent by this
    /// module's own question, but the wire format is untrusted input) must
    /// resolve to no catalogue route rather than a panic or a bogus
    /// rewrite.
    #[test]
    fn dispatch_tier_advise_is_none_on_an_unrecognised_tier_choice() {
        let body = r#"{"model": "jev-latest", "answers": {
            "tier": {"type": "choice", "choice": "ultra",
                     "probabilities": {"ultra": 0.9, "cheap": 0.1}, "confidence": 0.9}
        }, "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let (payload, tool_input) = agent_payload("general-purpose", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_UNKNOWN_TIER";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert!(output.is_none());
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    /// A `500` must fall through to the deterministic deny path with no
    /// effect recorded -- timeout and transport failure share the same
    /// `Err` arm in `jev::ask` and so the same fallback, already proven at
    /// the shared-client level (`jev.rs`); this is the one call-site-level
    /// proof that a non-`UnsafeState` error never blocks the deterministic
    /// deny.
    #[test]
    fn dispatch_tier_advise_is_none_on_a_500() {
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(500, "{}");
        let (payload, tool_input) = agent_payload("general-purpose", "", "implement the feature");
        let credential_env = "HOOK_TEST_JEV_DISPATCH_500";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let output = dispatch_tier_advise(&cfg, &state, "fable", &payload, &tool_input);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert!(output.is_none());
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    /// A named `.claude/agents/<name>.md` definition carries its own `model`
    /// frontmatter, so zirv has no business second-guessing it.
    #[test]
    fn a_named_custom_subagent_type_with_no_model_is_allowed() {
        for kind in [
            "vault-keeper",
            "statusline-setup",
            "claude-security:explore",
        ] {
            assert_eq!(
                decide(SEAT, "Agent", serde_json::json!({"subagent_type": kind})),
                None,
                "{kind} pins its own model"
            );
        }
    }

    #[test]
    fn the_generic_type_set_is_matched_exactly_and_not_by_prefix() {
        assert_eq!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose-reviewer"})
            ),
            None,
            "a custom type that merely starts like a generic one still pins its own model"
        );
    }

    #[test]
    fn both_spellings_of_the_subagent_tool_are_covered() {
        for tool in ["Agent", "Task"] {
            assert!(
                decide(
                    SEAT,
                    tool,
                    serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"})
                )
                .is_some(),
                "{tool} dispatches subagents"
            );
        }
    }

    #[test]
    fn the_seat_tier_test_is_case_insensitive_and_covers_mythos() {
        for seat in ["fable", "Fable", "claude-fable-5", "mythos", "MYTHOS[1m]"] {
            assert!(
                decide(
                    Some(seat),
                    "Agent",
                    serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"})
                )
                .is_some(),
                "{seat} is an expensive seat"
            );
        }
        for model in ["Fable", "us.anthropic.mythos-v1"] {
            assert!(
                decide(
                    SEAT,
                    "Agent",
                    serde_json::json!({"subagent_type": "general-purpose", "model": model, "prompt": "do the thing"})
                )
                .is_some(),
                "{model} names the seat tier"
            );
        }
    }

    // -- fail open ---------------------------------------------------------

    /// A payload naming the subagent tool but carrying no `tool_input` field
    /// at all is schema drift, not a dispatch -- `#[serde(default)]` still
    /// fills in `PreToolInput::default()`, and with no `prompt` in it the
    /// guard must not deny on those defaulted zero values.
    #[test]
    fn agent_tool_with_no_tool_input_field_at_all_is_allowed() {
        let payload = PreToolPayload::parse(r#"{"tool_name":"Agent"}"#)
            .expect("tool_input is optional at the top level");
        assert_eq!(pretool_decision(SEAT, &payload), None);
    }

    /// An explicit empty `tool_input: {}` is the same case as a missing one:
    /// no `prompt`, so nothing recognisable as a real dispatch.
    #[test]
    fn agent_tool_with_an_empty_tool_input_object_is_allowed() {
        assert_eq!(decide(SEAT, "Agent", serde_json::json!({})), None);
    }

    /// `tool_input` present, with fields that would have denied under the
    /// old rule (a fork naming the seat tier itself), but no `prompt` at
    /// all -- still not a recognisable dispatch, so this must fail open.
    /// This is the exact defect the `prompt` gate fixes: before it, this
    /// payload was denied on defaulted zero values alone.
    #[test]
    fn agent_tool_input_lacking_a_prompt_field_is_allowed() {
        assert_eq!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "fork", "model": "fable"})
            ),
            None,
            "no prompt means this payload is not recognised as a real dispatch"
        );
    }

    /// Regression: once a payload actually carries a `prompt`, the existing
    /// deny rule for an omitted model on a generic subagent type still
    /// applies exactly as it did before the `prompt` gate.
    #[test]
    fn a_prompt_carrying_dispatch_with_omitted_model_on_a_generic_type_is_still_denied() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "prompt": "do the thing"})
            )
            .is_some()
        );
    }

    #[test]
    fn with_no_seat_env_nothing_is_ever_denied() {
        assert_eq!(
            decide(None, "Agent", serde_json::json!({"subagent_type": "fork"})),
            None,
            "the guard is scoped to an expensive orchestrator seat and nothing else"
        );
    }

    #[test]
    fn a_cheap_seat_denies_nothing() {
        for seat in ["sonnet", "opus", "haiku", ""] {
            assert_eq!(
                decide(
                    Some(seat),
                    "Agent",
                    serde_json::json!({"subagent_type": "fork"})
                ),
                None,
                "a {seat} seat costs what a fork of it costs"
            );
        }
    }

    #[test]
    fn a_non_subagent_tool_is_never_touched() {
        for tool in ["Bash", "Read", "Edit", "WebFetch", "mcp__memory__create"] {
            assert_eq!(
                decide(SEAT, tool, serde_json::json!({"command": "ls"})),
                None,
                "{tool} spawns no seat-inheriting session"
            );
        }
    }

    /// A `tool_input` carrying types this hook does not model (numbers,
    /// nested objects, `run_in_background`) is the ordinary case for every
    /// tool that is not the subagent one, and must not be a parse failure
    /// that quietly turns the guard off for the tools it does model.
    #[test]
    fn an_unmodelled_tool_input_still_parses_and_allows() {
        let payload = PreToolPayload::parse(&pretool_stdin(
            "Bash",
            serde_json::json!({"command": "rm -rf /tmp/build", "timeout": 120000, "run_in_background": false}),
        ))
        .expect("an ordinary Bash payload must parse");
        assert_eq!(payload.tool_name, "Bash");
        assert_eq!(pretool_decision(SEAT, &payload), None);
    }

    #[test]
    fn pretool_payload_parses_and_defaults_subagent_identity() {
        let main_thread =
            PreToolPayload::parse(r#"{"tool_name":"Edit"}"#).expect("a main-thread payload parses");
        assert!(main_thread.agent_id.is_empty());
        assert!(main_thread.agent_type.is_empty());

        let subagent = PreToolPayload::parse(
            r#"{"tool_name":"Edit","agent_id":"a1b2","agent_type":"general-purpose"}"#,
        )
        .expect("a subagent payload parses");
        assert_eq!(subagent.agent_id, "a1b2");
        assert_eq!(subagent.agent_type, "general-purpose");
    }
}
