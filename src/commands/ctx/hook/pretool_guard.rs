//! PreToolUse advisories: the skill-pointer nudge and the orchestrator
//! seat's own write guard.

use std::path::{Path, PathBuf};

use super::checkpoints::cfg_or_operator_only_gate;
use super::pretool_tier::{PreToolPayload, pretool_intent, raw_tool_input, resolved_cwd};
use crate::commands::ctx::adapters::SESSION_ENV;
#[cfg(test)]
use crate::commands::ctx::config::env_from_process;
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::log;
use crate::commands::ctx::state::StateDir;

// -- PreToolUse: the subagent skill-library pointer (issue #539 chunk F
// derivative) --------------------------------------------------------------

/// Appended to an allowed `Agent`/`Task` dispatch's `prompt`, once, when
/// eligible ([`append_skill_pointer`]). Field evidence: a subagent never
/// inherits its parent's system prompt (and therefore never sees
/// `prompt::SKILL_INDEX_HEADER`/`skill_index_text`), so nothing today tells a
/// dispatched worker the skill library exists at all.
///
/// This is the library's EXISTENCE and the loading commands, never a
/// pre-selected skill -- issue #539 chunk F's standing operator decision
/// (`skill_activation.rs`'s own doc comment) is that zirv may make a skill's
/// existence deterministic but never the choice to use one, so this text
/// names no skill id and calls no scorer. ASCII only, no em dashes: every
/// other hook-adjacent string in this crate is held to that same rule.
pub(super) const SKILL_POINTER_NOTE: &str = "\n\n[zirv skills] This session's harness provides task skills \
(method and failure modes per task type). If you have a shell: before starting, run `zirv skill \
list --match \"<your task in a few words>\"`, run `zirv skill load <id>` for any that fits, and \
name the skills you loaded in your report.";

/// Whether [`SKILL_POINTER_NOTE`] should ride along with `prompt`: gated by
/// `cfg.prompt.skill_index` (the same switch that turns off the standing
/// skill-index system-prompt layer, `prompt::skill_index_text` -- `false`
/// means an operator wants no zirv-authored skill mention at all, standing
/// layer or per-dispatch pointer alike), and skipped when `prompt` already
/// mentions "zirv skill" (any casing) -- a parent that already briefed skills explicitly,
/// or a re-entrant hook -- so the pointer is never doubled.
fn wants_skill_pointer(cfg: &CtxConfig, prompt: &str) -> bool {
    cfg.prompt.skill_index && !prompt.to_ascii_lowercase().contains("zirv skill")
}

/// Appends [`SKILL_POINTER_NOTE`] to `tool_input`'s own `prompt` field IN
/// PLACE, when eligible, and reports whether it did. `tool_input` must
/// already be a JSON object carrying a string `prompt` -- anything else (not
/// an object, no `prompt`, or a non-string `prompt`) is left completely
/// untouched, which is exactly the subagent skill pointer's own scope: never
/// fire for a payload that is not a genuine dispatch with real task text.
pub(super) fn append_skill_pointer(tool_input: &mut serde_json::Value, cfg: &CtxConfig) -> bool {
    let Some(object) = tool_input.as_object_mut() else {
        return false;
    };
    let Some(serde_json::Value::String(prompt)) = object.get("prompt") else {
        return false;
    };
    if !wants_skill_pointer(cfg, prompt) {
        return false;
    }
    let mut updated = prompt.clone();
    updated.push_str(SKILL_POINTER_NOTE);
    object.insert("prompt".to_string(), serde_json::Value::String(updated));
    true
}

/// The plain `allow` envelope for a dispatch this hook never had anything
/// else to say about: `updatedInput` is the original `tool_input` with
/// [`SKILL_POINTER_NOTE`] appended to `prompt`, and nothing else -- no
/// `additionalContext`, mirroring [`pretool_dispatch_tier_output`]'s own
/// shape minus the note claude has no model rewrite to explain.
fn pretool_pointer_output(updated_input: serde_json::Value) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input
        }
    })
    .to_string()
}

/// The production wrapper for the plain (no model-rewrite) case: an
/// `Agent`/`Task` dispatch [`pretool_decision`] already allowed outright (or
/// never even reached, on a seat that guard does not gate at all), so this
/// hook printed nothing at all before the subagent skill pointer. Resolves
/// `cfg` from `env`/`payload.cwd` exactly as [`dispatch_tier_override`]
/// resolves its own, then appends the pointer when [`append_skill_pointer`]
/// says to. `None` -- meaning stay silent -- on a session with no
/// `SESSION_ENV` at all (gated the same way [`prompt_adoption_nudge`] gates
/// on it -- a non-empty value is the one signal common to every seat role
/// zirv supervises, unlike `SEAT_MODEL_ENV`, which only an orchestrator
/// carries), any non-dispatch tool, an unresolvable cwd, or an ineligible
/// prompt.
pub(super) fn skill_pointer_override(
    payload: &PreToolPayload,
    stdin: &str,
    env: EnvLookup<'_>,
) -> Option<String> {
    env(SESSION_ENV).filter(|session| !session.is_empty())?;
    if !crate::commands::ctx::lifecycle::SUBAGENT_TOOLS.contains(&payload.tool_name.as_str()) {
        return None;
    }
    let cwd = resolved_cwd(payload)?;
    let cfg = cfg_or_operator_only_gate(&cwd, env);
    let mut tool_input = raw_tool_input(stdin);
    append_skill_pointer(&mut tool_input, &cfg).then(|| pretool_pointer_output(tool_input))
}

// -- PreToolUse: the orchestrator-write guard (issues #328/#334) -----------

/// Tool names that write repository files. An orchestrator seat must be
/// technically unable to edit repository files itself -- every change goes
/// through a dispatched worker instead.
use crate::commands::ctx::lifecycle::FILE_MODIFICATION_TOOLS;

/// The absolute, lexically-normalized target `payload` names, or `None` when
/// the tool is not a [`FILE_MODIFICATION_TOOLS`] entry or the payload names
/// no target at all (schema drift, not a real write). A relative target is
/// resolved against `cwd` -- the caller's own already-resolved value (see
/// `run_pretool`: `payload.cwd`, falling back to the process cwd).
pub(super) fn normalized_write_target(payload: &PreToolPayload, cwd: &Path) -> Option<PathBuf> {
    if !FILE_MODIFICATION_TOOLS.contains(&payload.tool_name.as_str()) {
        return None;
    }
    let target = if !payload.tool_input.file_path.is_empty() {
        payload.tool_input.file_path.as_str()
    } else if !payload.tool_input.notebook_path.is_empty() {
        payload.tool_input.notebook_path.as_str()
    } else {
        return None;
    };
    let target = Path::new(target);
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        cwd.join(target)
    };
    Some(crate::commands::ctx::lifecycle::normalize_lexically(
        &resolved,
    ))
}

/// One orchestrator-write guard decision, resolved against this seat's own
/// posture. `Deny`/`Advise` carry the text for their own channel (a blocking
/// reason, a non-blocking advisory); `Allow` carries nothing -- the write
/// proceeds silently, though the caller still logs it so `zirv ctx status`
/// can count it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestratorWriteOutcome {
    Deny(String),
    Advise(String),
    Allow,
}

impl OrchestratorWriteOutcome {
    /// The `log::OrchestratorBlock::outcome` label for this decision --
    /// "denied"/"advised"/"allowed", matching `OrchestratorWrites::label`'s
    /// own three postures one-for-one.
    pub(crate) fn log_label(&self) -> &'static str {
        match self {
            OrchestratorWriteOutcome::Deny(_) => "denied",
            OrchestratorWriteOutcome::Advise(_) => "advised",
            OrchestratorWriteOutcome::Allow => "allowed",
        }
    }
}

/// How many prior "advised" rows this session already has in `log::
/// read_orchestrator_blocks` before an advisory note surfaces again (issue
/// #358 T8): the write itself is never blocked by this -- only whether the
/// hook's own non-blocking note rides along -- so a rate limit here trades
/// visibility for quiet, never safety for quiet. `0`, `N`, `2N`, ... each
/// surface a note; everything between stays silent. Shared by both
/// `hook::run_pretool` (Edit/Write/MultiEdit/NotebookEdit) and `safety::
/// run_check_hook_mode_with_env` (Bash/PowerShell), which count the SAME
/// session's rows in the SAME log, so an operator alternating between tool
/// families still only sees a note every fifth orchestrator write, not
/// every fifth per family.
pub(super) const ORCHESTRATOR_ADVISORY_RATE: usize = 5;

/// Whether this session's next `Advise`-posture write should carry a
/// surfaced advisory note, based on how many `outcome == "advised"` rows it
/// already has. Best-effort like every other log read here: a `StateDir`
/// that fails to resolve, or a log that fails to read, degrades to `true`
/// (surface it) rather than silently going quiet -- the annoyance of an
/// extra note is a far cheaper failure mode than a session that never
/// learns it should be delegating more.
///
/// Only the ledger's TAIL is parsed (`log::read_recent_orchestrator_blocks`):
/// this runs on every Edit/Write/Bash hook and the file is never rotated, so
/// a full read would grow without bound on the hottest path there is. A
/// session whose own rows all sit inside the window -- every ordinary
/// session -- counts exactly as it did before; one whose rows are older than
/// the window simply starts its cadence over, which can only surface a note
/// that would otherwise have been suppressed, never suppress one.
pub(crate) fn orchestrator_advisory_should_surface(env: EnvLookup<'_>, session: &str) -> bool {
    let Ok(state) = StateDir::resolve(env) else {
        return true;
    };
    let count = log::read_recent_orchestrator_blocks(&state)
        .iter()
        .filter(|row| row.session == session && row.outcome == "advised")
        .count();
    count % ORCHESTRATOR_ADVISORY_RATE == 0
}

/// The resolved write TARGET when `payload` is an orchestrator seat's own
/// in-scope repository write, or `None` when it is outside this guard's
/// scope entirely (and so gets no [`OrchestratorWriteOutcome`] at all --
/// not even `Allow` -- because there is nothing here for a posture to act
/// on). `role` is `SEAT_ROLE_ENV`'s value.
///
/// Confinement is anchored on the resolved TARGET, never on `cwd` or the
/// launch repo: an orchestrator seat has no business editing source in ANY
/// git repository, including a sibling checkout or a linked worktree of a
/// repository entirely unrelated to the one it was launched in (review
/// finding on issue #334) -- so `repo_root_for_target` finds the repo the
/// target itself sits in, and the exemption is narrowed only against THAT
/// repo's own `<target_repo>/.zirv/work`/`<target_repo>/.zirv/memory` --
/// the two roots a worker's own dispatch/handoff/memory writes still need
/// from this seat. Claude Code's own harness home (`CLAUDE_CONFIG_DIR`, or
/// `$HOME/.claude`/`%USERPROFILE%\\.claude`) is outside repository-write
/// classification even when an ancestor carries `.git`. A target that sits in no git repository at all
/// is outside this guard's scope. Every other gate below is also out of
/// scope: a non-orchestrator role, a native subagent call (`agent_id` is
/// non-empty), a tool that is not a [`FILE_MODIFICATION_TOOLS`] entry, or an
/// empty target (schema drift, not a real write).
fn orchestrator_write_target(
    role: Option<&str>,
    payload: &PreToolPayload,
    cwd: &Path,
    env: EnvLookup<'_>,
) -> Option<PathBuf> {
    // Issue #478: the rule itself lives in `lifecycle.rs` so a native
    // session's own `file_write`/`apply_patch` call reaches it too; this stays
    // the translator that resolves claude's `file_path`/`notebook_path`
    // against `cwd`.
    let mut intent = pretool_intent(payload);
    intent.write_target = normalized_write_target(payload, cwd);
    crate::commands::ctx::lifecycle::orchestrator_write_target(role, &intent, env)
}

/// The whole orchestrator-write guard decision (issue #358 T8): `None` when
/// [`orchestrator_write_target`] finds this call outside the guard's scope
/// (nothing to log, nothing to decide); otherwise `Some` of this seat's own
/// posture applied to that target -- `Deny`/`Advise` carry their own
/// channel's text, `Allow` carries nothing. `role`/`cwd`/`env` are exactly
/// [`orchestrator_write_target`]'s own; `posture` is `hook::
/// orchestrator_write_posture`'s resolved value.
pub fn orchestrator_write_decision(
    role: Option<&str>,
    payload: &PreToolPayload,
    cwd: &Path,
    env: EnvLookup<'_>,
    posture: crate::commands::ctx::config::OrchestratorWrites,
) -> Option<OrchestratorWriteOutcome> {
    use crate::commands::ctx::config::OrchestratorWrites;
    let target = orchestrator_write_target(role, payload, cwd, env)?;
    Some(match posture {
        OrchestratorWrites::Deny => OrchestratorWriteOutcome::Deny(
            crate::commands::ctx::lifecycle::orchestrator_write_deny_reason(&target),
        ),
        OrchestratorWrites::Advise => OrchestratorWriteOutcome::Advise(
            crate::commands::ctx::lifecycle::orchestrator_write_advise_note(&target),
        ),
        OrchestratorWrites::Allow => OrchestratorWriteOutcome::Allow,
    })
}

#[cfg(test)]
mod tests {
    use super::super::pretool_tier::PreToolPayload;
    use super::super::tests::{orchestrator_pretool_stdin, orchestrator_repo};
    use super::*;
    use crate::commands::ctx::config::OrchestratorWrites;

    /// `orchestrator-blocks.jsonl` is never rotated and is read on EVERY
    /// Edit/Write/Bash hook, so the advisory rate limit may only look at a
    /// bounded tail of it. A ledger whose head carries this session's own
    /// ancient rows must give the verdict the tail alone implies -- a
    /// whole-file read would count those too and go silent instead.
    #[test]
    fn orchestrator_advisory_only_reads_the_tail_of_the_block_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let env = |key: &str| {
            (key == crate::commands::ctx::state::STATE_ENV)
                .then(|| dir.path().display().to_string())
        };
        fn row<'a>(session: &'a str, outcome: &'a str, ts: u64) -> log::OrchestratorBlock<'a> {
            log::OrchestratorBlock {
                ts,
                session,
                tool: "Edit",
                target: "src/lib.rs",
                reason: "orchestrator seat",
                outcome,
            }
        }

        for i in 0..2 {
            log::append_orchestrator_block(&state, &row("mine", "advised", i)).expect("append");
        }
        for i in 0..10_000 {
            log::append_orchestrator_block(&state, &row("other", "advised", 100 + i))
                .expect("append");
        }
        for i in 0..5 {
            log::append_orchestrator_block(&state, &row("mine", "advised", 20_000 + i))
                .expect("append");
        }

        let bytes = std::fs::metadata(state.logs().join(log::ORCHESTRATOR_BLOCKS_FILE))
            .expect("metadata")
            .len();
        assert!(
            bytes > log::ORCHESTRATOR_BLOCK_TAIL_BYTES,
            "the fixture must exceed the tail window to be a test of it ({bytes} bytes)"
        );
        assert!(
            orchestrator_advisory_should_surface(&env, "mine"),
            "5 advised rows inside the tail window is a multiple of the rate; the 2 ancient \
             rows before it must not be counted"
        );
    }

    /// The bounded read may not change the verdict for the ordinary case:
    /// a ledger small enough to fit the tail window entirely counts exactly
    /// as it always did.
    #[test]
    fn orchestrator_advisory_verdict_is_unchanged_for_a_small_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let env = |key: &str| {
            (key == crate::commands::ctx::state::STATE_ENV)
                .then(|| dir.path().display().to_string())
        };
        fn row<'a>(session: &'a str, outcome: &'a str, ts: u64) -> log::OrchestratorBlock<'a> {
            log::OrchestratorBlock {
                ts,
                session,
                tool: "Edit",
                target: "src/lib.rs",
                reason: "orchestrator seat",
                outcome,
            }
        }

        assert!(
            orchestrator_advisory_should_surface(&env, "mine"),
            "an empty ledger surfaces the first note"
        );
        log::append_orchestrator_block(&state, &row("other", "advised", 1)).expect("append");
        log::append_orchestrator_block(&state, &row("mine", "denied", 2)).expect("append");
        log::append_orchestrator_block(&state, &row("mine", "advised", 3)).expect("append");
        assert!(
            !orchestrator_advisory_should_surface(&env, "mine"),
            "one advised row of this session's own stays quiet"
        );
        for ts in 4..8 {
            log::append_orchestrator_block(&state, &row("mine", "advised", ts)).expect("append");
        }
        assert!(
            orchestrator_advisory_should_surface(&env, "mine"),
            "the fifth surfaces again"
        );
    }

    /// Idempotency: a parent that already briefed skills explicitly (or a
    /// re-entrant hook that already appended the note once) must not get it
    /// doubled.
    #[test]
    fn wants_skill_pointer_is_off_when_the_prompt_already_mentions_zirv_skill() {
        let cfg = CtxConfig::default();
        assert!(!wants_skill_pointer(
            &cfg,
            "before you start, run zirv skill list --match \"...\""
        ));
        assert!(!wants_skill_pointer(
            &cfg,
            "See the Zirv Skill index first."
        ));
    }

    /// `prompt.skill_index = false` turns off both the standing skill-index
    /// system-prompt layer (`prompt::skill_index_text`) and this per-dispatch
    /// pointer -- an operator who wants no zirv-authored skill mention at all
    /// gets exactly that.
    #[test]
    fn wants_skill_pointer_is_off_when_skill_index_is_disabled() {
        let mut cfg = CtxConfig::default();
        cfg.prompt.skill_index = false;
        assert!(!wants_skill_pointer(&cfg, "implement the feature"));
    }

    /// `append_skill_pointer` must never fire for a payload that is not a
    /// genuine dispatch: no `prompt` key at all, a `prompt` of the wrong JSON
    /// type, or a `tool_input` that is not even an object -- all schema
    /// drift, not a real dispatch.
    #[test]
    fn append_skill_pointer_ignores_a_missing_or_non_string_prompt() {
        let cfg = CtxConfig::default();
        let mut no_prompt = serde_json::json!({"subagent_type": "general-purpose"});
        assert!(!append_skill_pointer(&mut no_prompt, &cfg));
        assert_eq!(
            no_prompt,
            serde_json::json!({"subagent_type": "general-purpose"})
        );

        let mut wrong_type = serde_json::json!({"prompt": 42});
        assert!(!append_skill_pointer(&mut wrong_type, &cfg));
        assert_eq!(wrong_type, serde_json::json!({"prompt": 42}));

        let mut not_an_object = serde_json::json!("do the thing");
        assert!(!append_skill_pointer(&mut not_an_object, &cfg));
        assert_eq!(not_an_object, serde_json::json!("do the thing"));
    }

    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("repo dir");
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
    }

    fn edit_payload(repo: &Path, relative_target: &str) -> PreToolPayload {
        let file_path = repo.join(relative_target);
        PreToolPayload::parse(&orchestrator_pretool_stdin(
            &repo.display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": file_path.display().to_string()}),
        ))
        .expect("the documented payload must parse")
    }

    #[test]
    fn orchestrator_write_decision_denies_an_edit_under_the_repo() {
        let repo = orchestrator_repo();
        let payload = edit_payload(repo.path(), "src/x.rs");
        let outcome = orchestrator_write_decision(
            Some("orchestrator"),
            &payload,
            repo.path(),
            &|_| None,
            OrchestratorWrites::Deny,
        )
        .expect("an orchestrator editing a repo file must be denied");
        let OrchestratorWriteOutcome::Deny(reason) = outcome else {
            panic!("expected a Deny outcome, got {outcome:?}");
        };
        assert!(
            reason.contains("orchestrator seat: dispatch a worker"),
            "{reason}"
        );
    }

    #[test]
    fn orchestrator_write_decision_allows_an_edit_under_the_default_harness_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let _config = crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CONFIG_DIR", None)]);
        let harness_home = home.path().join(".claude");
        init_git_repo(&harness_home);
        let target = harness_home.join("projects/slug/memory/note.md");
        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &home.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": target.display().to_string()}),
        ))
        .expect("payload parses");
        let env = env_from_process();

        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                home.path(),
                &env,
                OrchestratorWrites::Deny,
            ),
            None
        );
    }

    #[test]
    fn default_harness_home_uses_userprofile_when_home_is_absent() {
        let profile = tempfile::tempdir().expect("tempdir");
        let harness_home = profile.path().join(".claude");
        std::fs::create_dir_all(&harness_home).expect("harness home");
        let target = harness_home.join("projects/slug/memory/note.md");
        let profile = profile.path().display().to_string();
        let env = |key: &str| match key {
            "USERPROFILE" => Some(profile.clone()),
            _ => None,
        };

        assert!(crate::commands::ctx::lifecycle::target_is_under_harness_home(&target, &env));
    }

    #[test]
    fn orchestrator_write_decision_allows_an_edit_under_a_configured_harness_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let configured = home.path().join("cfg");
        init_git_repo(&configured);
        let configured_value = configured.to_string_lossy();
        let _config = crate::commands::ctx::testenv::VarGuard::set(&[(
            "CLAUDE_CONFIG_DIR",
            Some(configured_value.as_ref()),
        )]);
        let target = configured.join("projects/slug/memory/MEMORY.md");
        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &home.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": target.display().to_string()}),
        ))
        .expect("payload parses");
        let env = env_from_process();

        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                home.path(),
                &env,
                OrchestratorWrites::Deny,
            ),
            None
        );
    }

    #[test]
    fn orchestrator_write_decision_still_denies_a_repo_elsewhere_under_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let _config = crate::commands::ctx::testenv::VarGuard::set(&[("CLAUDE_CONFIG_DIR", None)]);
        let repo = home.path().join("repo");
        init_git_repo(&repo);
        let target = repo.join("src/x.rs");
        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &repo.display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": target.display().to_string()}),
        ))
        .expect("payload parses");
        let env = env_from_process();

        assert!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                &repo,
                &env,
                OrchestratorWrites::Deny,
            )
            .is_some(),
            "a repository outside the harness home must stay denied"
        );
    }

    #[test]
    fn orchestrator_write_decision_allows_a_native_subagent_edit() {
        let repo = orchestrator_repo();
        let mut payload = edit_payload(repo.path(), "src/x.rs");
        payload.agent_id = "a1b2".to_string();

        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            ),
            None,
            "a native subagent is the worker this guard asks the seat to dispatch"
        );
        assert_eq!(
            orchestrator_write_decision(
                Some("worker"),
                &payload,
                repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            ),
            None,
            "subagent identity must not change non-orchestrator behavior"
        );
    }

    #[test]
    fn orchestrator_write_decision_allows_every_non_orchestrator_role() {
        let repo = orchestrator_repo();
        let payload = edit_payload(repo.path(), "src/x.rs");
        for role in [None, Some("worker"), Some("sub-orchestrator")] {
            assert_eq!(
                orchestrator_write_decision(
                    role,
                    &payload,
                    repo.path(),
                    &|_| None,
                    OrchestratorWrites::Deny,
                ),
                None,
                "{role:?} must never be blocked from editing"
            );
        }
    }

    #[test]
    fn orchestrator_write_decision_allows_zirv_work_and_memory_writes() {
        let repo = orchestrator_repo();
        for relative in [".zirv/work/notes.md", ".zirv/memory/x.md"] {
            let payload = edit_payload(repo.path(), relative);
            assert_eq!(
                orchestrator_write_decision(
                    Some("orchestrator"),
                    &payload,
                    repo.path(),
                    &|_| None,
                    OrchestratorWrites::Deny,
                ),
                None,
                "{relative} must stay allowed"
            );
        }
    }

    #[test]
    fn orchestrator_write_decision_resolves_a_relative_target_against_cwd() {
        let repo = orchestrator_repo();
        let inside = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &repo.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": "src/x.rs"}),
        ))
        .expect("payload parses");
        assert!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &inside,
                repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            )
            .is_some(),
            "a relative target must resolve against cwd, landing inside the repo"
        );

        let outside = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &repo.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": "../outside.txt"}),
        ))
        .expect("payload parses");
        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &outside,
                repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            ),
            None,
            "a relative target that climbs outside the repo must be allowed"
        );
    }

    #[test]
    fn orchestrator_write_decision_treats_an_empty_target_as_schema_drift() {
        let repo = orchestrator_repo();
        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &repo.path().display().to_string(),
            "claude-session-id",
            "Write",
            serde_json::json!({"file_path": ""}),
        ))
        .expect("payload parses");
        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            ),
            None,
            "an empty target is schema drift, not a real write"
        );
    }

    #[test]
    fn orchestrator_write_decision_denies_a_notebook_edit_under_the_repo() {
        let repo = orchestrator_repo();
        let notebook_path = repo.path().join("nb.ipynb");
        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &repo.path().display().to_string(),
            "claude-session-id",
            "NotebookEdit",
            serde_json::json!({"notebook_path": notebook_path.display().to_string()}),
        ))
        .expect("payload parses");
        assert!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            )
            .is_some()
        );
    }

    #[test]
    fn orchestrator_write_decision_ignores_read_and_bash() {
        let repo = orchestrator_repo();
        let file_path = repo.path().join("src/x.rs");
        for (tool, input) in [
            (
                "Read",
                serde_json::json!({"file_path": file_path.display().to_string()}),
            ),
            ("Bash", serde_json::json!({"command": "cat src/x.rs"})),
        ] {
            let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
                &repo.path().display().to_string(),
                "claude-session-id",
                tool,
                input,
            ))
            .expect("payload parses");
            assert_eq!(
                orchestrator_write_decision(
                    Some("orchestrator"),
                    &payload,
                    repo.path(),
                    &|_| None,
                    OrchestratorWrites::Deny,
                ),
                None,
                "{tool} is not a file-modification tool"
            );
        }
    }

    /// Review finding on issue #334: confinement is anchored on the TARGET,
    /// not on `cwd`/the launch repo -- editing a SIBLING checkout or a
    /// linked worktree of an entirely different repository must still be
    /// denied, even though it sits nowhere under `cwd`.
    #[test]
    fn orchestrator_write_decision_denies_a_target_inside_a_different_repo_than_cwd() {
        let launch_repo = orchestrator_repo();
        let sibling = tempfile::tempdir().expect("sibling tempdir");
        // A linked worktree of some other repository: `.git` is a FILE.
        std::fs::write(
            sibling.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/wt\n",
        )
        .expect(".git file");
        let target = sibling.path().join("src").join("foo.rs");

        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &launch_repo.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": target.display().to_string()}),
        ))
        .expect("payload parses");

        let outcome = orchestrator_write_decision(
            Some("orchestrator"),
            &payload,
            launch_repo.path(),
            &|_| None,
            OrchestratorWrites::Deny,
        )
        .expect("a sibling checkout's own source must be denied too");
        let OrchestratorWriteOutcome::Deny(reason) = outcome else {
            panic!("expected a Deny outcome, got {outcome:?}");
        };
        assert!(
            reason.contains("orchestrator seat: dispatch a worker"),
            "{reason}"
        );
    }

    /// A target that sits in no git repository at all is outside this
    /// guard's scope: there is no "repository file" here to protect. Uses a
    /// fresh tempdir rather than the raw OS temp root, which on some
    /// machines is itself inside a git checkout.
    #[test]
    fn orchestrator_write_decision_allows_a_target_with_no_git_ancestor_at_all() {
        let launch_repo = orchestrator_repo();
        let no_git = tempfile::tempdir().expect("tempdir with no .git anywhere above it");
        let target = no_git.path().join("scratch.txt");

        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &launch_repo.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": target.display().to_string()}),
        ))
        .expect("payload parses");

        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                launch_repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            ),
            None,
            "a target outside any git repository is outside this guard's scope"
        );
    }

    /// `.zirv/work` inside a SIBLING repo -- not the launch repo -- must
    /// stay allowed too: the exemption is scoped to the target's own
    /// repository, never hardcoded to whichever repo the seat launched in.
    #[test]
    fn orchestrator_write_decision_allows_zirv_work_inside_a_sibling_repo() {
        let launch_repo = orchestrator_repo();
        let sibling = orchestrator_repo();
        let target = sibling.path().join(".zirv").join("work").join("x.md");

        let payload = PreToolPayload::parse(&orchestrator_pretool_stdin(
            &launch_repo.path().display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": target.display().to_string()}),
        ))
        .expect("payload parses");

        assert_eq!(
            orchestrator_write_decision(
                Some("orchestrator"),
                &payload,
                launch_repo.path(),
                &|_| None,
                OrchestratorWrites::Deny,
            ),
            None,
            "a sibling repo's own .zirv/work stays allowed"
        );
    }

    #[test]
    fn repo_root_for_target_finds_a_git_directory_ancestor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect(".git dir");
        // The target itself need not exist -- a `Write` target may not yet.
        let target = repo.join("src").join("deep").join("new_file.rs");
        assert_eq!(
            crate::commands::ctx::lifecycle::repo_root_for_target(&target),
            Some(repo)
        );
    }

    #[test]
    fn repo_root_for_target_finds_a_git_file_ancestor_for_a_linked_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        std::fs::write(
            worktree.join(".git"),
            "gitdir: /elsewhere/.git/worktrees/wt\n",
        )
        .expect(".git file");
        let target = worktree.join("src").join("new_file.rs");
        assert_eq!(
            crate::commands::ctx::lifecycle::repo_root_for_target(&target),
            Some(worktree)
        );
    }

    #[test]
    fn repo_root_for_target_is_none_with_no_git_ancestor_at_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let plain = tmp.path().join("no-git-here");
        let target = plain.join("new_file.rs");
        assert_eq!(
            crate::commands::ctx::lifecycle::repo_root_for_target(&target),
            None
        );
    }
}
