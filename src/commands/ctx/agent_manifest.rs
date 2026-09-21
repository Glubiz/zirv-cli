//! `zirv ctx agent --manifest <path>`: one declarative delegation file
//! instead of the long flag list on [`AgentArgs`] (issue #725).
//!
//! # Trust boundary
//!
//! The manifest is the SAME kind of input as any other repo-owned surface
//! (CLAUDE.md's trust-boundary rule): UNTRUSTED, and may only NARROW. It is
//! parsed here into plain [`AgentArgs`] field values and nothing else --
//! [`apply`] hands the merged `AgentArgs` back to `run_with`, which then
//! runs the SAME validation/`envelope::narrow`/`coordinator::check` gates
//! every other launch already goes through. This module adds no bypass and
//! no new grant path: a manifest can never make a delegation wider than the
//! same flags typed on the CLI could have.
//!
//! Merge rule, when both the CLI and the manifest set the same field:
//! - Narrowing-capable fields (`no_network`, `budget_tokens`,
//!   `max_tool_calls`, `path_scope`, read-only `mode`) -- the STRICTER
//!   value wins regardless of which side set it.
//! - Plain identity fields (`brief`/the positional prompt, `task`, `group`,
//!   `workdir`, the result contract) -- an explicit CLI value that DIFFERS
//!   from the manifest's is a hard error naming both values. Equal values
//!   are fine.
//!
//! Relative paths inside the manifest (`workdir`, `result.schema`, each
//! `path_scope` entry) resolve relative to the manifest FILE's own
//! directory, then flow into the identical `AgentArgs` field a CLI-typed
//! path would -- so a path escaping what the CLI would allow is refused by
//! the SAME downstream checks (`validate_workdir`, `resolve_result_schema`,
//! `envelope::narrow`) a CLI-typed path already goes through, not by new
//! code here.
//!
//! # Non-goal: `agent:` (an `AgentManifest` id)
//!
//! The issue proposed an `agent: <AgentManifest id>` field resolved via
//! `workflow::agents::AgentRegistry::get`, the same registry the native
//! `delegate` MCP tool already uses. The harness-runtime `zirv ctx agent`
//! path (`agent.rs`) has no notion of `AgentManifest` at all today: no
//! `model_tier`-to-harness-model mapping, no capability/skill gating, and
//! `--role` here means "worker" or "sub-orchestrator" (`validate_role`),
//! not the organizational role an `AgentManifest` carries. Wiring `agent:`
//! in here would mean half-implementing that native-only machinery rather
//! than reusing it, so v1 drops it: this manifest is declarative
//! `AgentArgs` only.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::CtxResult;
use super::agent::AgentArgs;
use super::permit::WorkerMode;

/// Mirrors `workflow/definition.rs::MAX_DEFINITION_BYTES` / `workflow/
/// agents.rs::MAX_MANIFEST_BYTES`: a delegation manifest is a small,
/// hand-reviewable file, not a place to smuggle an unbounded blob.
const MAX_MANIFEST_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestResult {
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// Field names mirror `AgentArgs`'s own long flags 1:1 -- no new
/// vocabulary. `agent` (see this module's doc comment) is deliberately
/// absent from v1.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegationManifest {
    #[serde(default)]
    brief: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default)]
    mode: Option<WorkerMode>,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    max_tool_calls: Option<u32>,
    #[serde(default)]
    path_scope: Vec<PathBuf>,
    #[serde(default)]
    no_network: bool,
    #[serde(default)]
    result: Option<ManifestResult>,
}

/// The one call site (`agent::run_with`'s first line): a no-op when
/// `args.manifest` is `None`, otherwise loads and merges it into `args` in
/// place before anything else in `run_with` reads a field it can touch.
pub fn apply(args: &mut AgentArgs) -> CtxResult<()> {
    let Some(path) = args.manifest.clone() else {
        return Ok(());
    };
    let manifest = load(&path)?;
    let manifest_dir = path.parent().unwrap_or_else(|| Path::new("."));
    merge(args, manifest, manifest_dir)
}

fn load(path: &Path) -> CtxResult<DelegationManifest> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("--manifest '{}': {error}", path.display()))?;
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size > MAX_MANIFEST_BYTES {
        return Err(format!(
            "--manifest '{}' is {size} bytes; limit is {MAX_MANIFEST_BYTES}",
            path.display()
        )
        .into());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("--manifest '{}': {error}", path.display()))?;
    serde_yaml_ng::from_str(&text)
        .map_err(|error| format!("invalid --manifest '{}': {error}", path.display()).into())
}

fn resolve_path(manifest_dir: &Path, value: PathBuf) -> PathBuf {
    if value.is_absolute() {
        value
    } else {
        manifest_dir.join(value)
    }
}

fn merge(args: &mut AgentArgs, manifest: DelegationManifest, manifest_dir: &Path) -> CtxResult<()> {
    // `brief` fills the positional prompt only when it was left unstated
    // (empty) -- see `exec.rs`'s own `prompt.trim().is_empty()` check for
    // the same "empty means unstated" convention on this codebase's other
    // prompt-carrying flag.
    if let Some(brief) = manifest.brief {
        if args.prompt.is_empty() {
            args.prompt = brief;
        } else if args.prompt != brief {
            return Err(format!(
                "the positional prompt ('{}') differs from --manifest brief ('{brief}')",
                args.prompt
            )
            .into());
        }
    }

    merge_identity(&mut args.task, manifest.task, "--task", "--manifest task")?;
    merge_identity(
        &mut args.group,
        manifest.group,
        "--group",
        "--manifest group",
    )?;

    if let Some(workdir) = manifest.workdir {
        let workdir = resolve_path(manifest_dir, workdir);
        match &args.workdir {
            Some(existing) if *existing != workdir => {
                return Err(format!(
                    "--workdir ('{}') differs from --manifest workdir ('{}')",
                    existing.display(),
                    workdir.display()
                )
                .into());
            }
            Some(_) => {}
            None => args.workdir = Some(workdir),
        }
    }

    if let Some(result) = manifest.result {
        merge_result(args, result, manifest_dir)?;
    }

    // Narrowing-capable fields: the stricter value wins regardless of
    // source. Each already defaults to its LEAST restrictive value when
    // unstated on the CLI (`WorkerMode::Writing`, `false`, `None`, an
    // empty `Vec`) -- there is no way to tell "the CLI default" from "the
    // CLI explicitly asked for the least restriction", and no need to: the
    // least restrictive value loses to anything stricter regardless of
    // which side stated it.
    if manifest.mode == Some(WorkerMode::ReadOnly) {
        args.mode = WorkerMode::ReadOnly;
    }
    args.no_network = args.no_network || manifest.no_network;
    args.budget_tokens = stricter_ceiling(args.budget_tokens, manifest.budget_tokens);
    args.max_tool_calls = stricter_ceiling(args.max_tool_calls, manifest.max_tool_calls);
    if !manifest.path_scope.is_empty() {
        let resolved: Vec<PathBuf> = manifest
            .path_scope
            .into_iter()
            .map(|value| resolve_path(manifest_dir, value))
            .collect();
        let cli = std::mem::take(&mut args.path_scope);
        args.path_scope = stricter_path_scope(cli, resolved);
    }

    Ok(())
}

/// Rule 2's plain-identity-field case: an explicit CLI value differing
/// from the manifest's is a hard error naming both; equal values, or only
/// one side setting the field, are both fine.
fn merge_identity(
    field: &mut Option<String>,
    manifest_value: Option<String>,
    cli_label: &str,
    manifest_label: &str,
) -> CtxResult<()> {
    let Some(manifest_value) = manifest_value else {
        return Ok(());
    };
    match field {
        Some(existing) if *existing != manifest_value => Err(format!(
            "{cli_label} ('{existing}') differs from {manifest_label} ('{manifest_value}')"
        )
        .into()),
        Some(_) => Ok(()),
        None => {
            *field = Some(manifest_value);
            Ok(())
        }
    }
}

fn merge_result(
    args: &mut AgentArgs,
    result: ManifestResult,
    manifest_dir: &Path,
) -> CtxResult<()> {
    match (result.schema, result.kind) {
        (Some(_), Some(_)) => {
            Err("--manifest result: `schema` and `kind` are mutually exclusive".into())
        }
        (None, None) => Err("--manifest result: one of `schema` or `kind` is required".into()),
        (Some(schema), None) => {
            let resolved = resolve_path(manifest_dir, PathBuf::from(schema))
                .display()
                .to_string();
            if let Some(kind) = &args.result_kind {
                return Err(format!(
                    "--result-kind ('{kind}') differs from --manifest result.schema ('{resolved}')"
                )
                .into());
            }
            merge_identity(
                &mut args.result_schema,
                Some(resolved),
                "--result-schema",
                "--manifest result.schema",
            )
        }
        (None, Some(kind)) => {
            if let Some(schema) = &args.result_schema {
                return Err(format!(
                    "--result-schema ('{schema}') differs from --manifest result.kind ('{kind}')"
                )
                .into());
            }
            merge_identity(
                &mut args.result_kind,
                Some(kind),
                "--result-kind",
                "--manifest result.kind",
            )
        }
    }
}

fn stricter_ceiling<T: Ord + Copy>(cli: Option<T>, manifest: Option<T>) -> Option<T> {
    match (cli, manifest) {
        (Some(a), Some(b)) => Some(if a < b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// `Vec::new()` on either side means "unstated" (there is no way to spell
/// an explicit, empty `--path-scope` -- it is a repeatable flag), so an
/// empty side always defers to the other. When both narrow explicitly, the
/// strictest defensible outcome is whichever set the other actually
/// contains, or their intersection when neither does -- always at least as
/// narrow as either input alone.
fn stricter_path_scope(cli: Vec<PathBuf>, manifest: Vec<PathBuf>) -> Vec<PathBuf> {
    if cli.is_empty() {
        return manifest;
    }
    if manifest.is_empty() {
        return cli;
    }
    if manifest.iter().all(|path| cli.contains(path)) {
        return manifest;
    }
    if cli.iter().all(|path| manifest.contains(path)) {
        return cli;
    }
    cli.into_iter()
        .filter(|path| manifest.contains(path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(name: &str, prompt: &str) -> AgentArgs {
        AgentArgs {
            name: name.to_string(),
            prompt: prompt.to_string(),
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
            attach_artifact: None,
            workflow: None,
            task_class: None,
            result_schema: None,
            result_kind: None,
            path_scope: Vec::new(),
            no_network: false,
            depth: None,
            task: None,
            json: false,
            runtime: super::super::runtime::RuntimeKind::Harness.to_string(),
            route: None,
            manifest: None,
            session_id: None,
            cancellation: None,
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The issue's own acceptance criterion: a fixture manifest resolves
    /// `AgentArgs` to exactly what the equivalent flag list would have,
    /// including relative-path resolution against the manifest's own
    /// directory (rule 3).
    #[test]
    fn a_fixture_manifest_resolves_to_the_equivalent_flag_list() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("nested").join("delegation.yaml");
        write(
            &manifest_path,
            "brief: do the operator's own thing\n\
             task: task-123\n\
             group: wg-9\n\
             workdir: workdir\n\
             mode: read-only\n\
             budget_tokens: 500\n\
             max_tool_calls: 10\n\
             path_scope:\n  - scoped\n\
             no_network: true\n\
             result:\n  kind: review\n",
        );
        let manifest_dir = manifest_path.parent().unwrap().to_path_buf();

        let mut actual = args_for("claude", "");
        actual.manifest = Some(manifest_path);
        apply(&mut actual).expect("a well-formed manifest applies");

        let expected = AgentArgs {
            task: Some("task-123".to_string()),
            group: Some("wg-9".to_string()),
            workdir: Some(manifest_dir.join("workdir")),
            mode: WorkerMode::ReadOnly,
            budget_tokens: Some(500),
            max_tool_calls: Some(10),
            path_scope: vec![manifest_dir.join("scoped")],
            no_network: true,
            result_kind: Some("review".to_string()),
            ..args_for("claude", "do the operator's own thing")
        };

        assert_eq!(actual.prompt, expected.prompt);
        assert_eq!(actual.task, expected.task);
        assert_eq!(actual.group, expected.group);
        assert_eq!(actual.workdir, expected.workdir);
        assert_eq!(actual.mode, expected.mode);
        assert_eq!(actual.budget_tokens, expected.budget_tokens);
        assert_eq!(actual.max_tool_calls, expected.max_tool_calls);
        assert_eq!(actual.path_scope, expected.path_scope);
        assert_eq!(actual.no_network, expected.no_network);
        assert_eq!(actual.result_kind, expected.result_kind);
        assert_eq!(actual.result_schema, expected.result_schema);
        // Fields the manifest never touches stay byte for byte what the
        // caller already had.
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.role, expected.role);
        assert_eq!(actual.runtime, expected.runtime);
    }

    #[test]
    fn apply_is_a_no_op_when_no_manifest_is_set() {
        let mut args = args_for("claude", "go");
        apply(&mut args).expect("no manifest is always fine");
        assert_eq!(args.prompt, "go");
        assert_eq!(args.task, None);
    }

    #[test]
    fn an_unknown_manifest_field_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("bad.yaml");
        write(&manifest_path, "brief: go\nagent: implementer\n");

        let mut args = args_for("claude", "");
        args.manifest = Some(manifest_path);
        let error = apply(&mut args).expect_err("agent: is not v1 vocabulary");
        assert!(error.to_string().contains("agent"), "got {error}");
    }

    /// Rule 2: a plain identity field where the CLI and the manifest
    /// disagree is a hard error naming both values, never a silent pick.
    #[test]
    fn a_differing_identity_field_is_a_hard_error_naming_both_values() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "task: task-from-manifest\n");

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        args.task = Some("task-from-cli".to_string());
        let error = apply(&mut args).expect_err("differing --task must be refused");
        let message = error.to_string();
        assert!(message.contains("task-from-cli"), "got {message}");
        assert!(message.contains("task-from-manifest"), "got {message}");
    }

    /// Equal identity values across the CLI and the manifest are fine.
    #[test]
    fn an_identical_identity_field_on_both_sides_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "task: same-task\n");

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        args.task = Some("same-task".to_string());
        apply(&mut args).expect("identical values never conflict");
        assert_eq!(args.task.as_deref(), Some("same-task"));
    }

    /// Stricter-wins, manifest direction: the CLI leaves `no_network`
    /// unstated (`false`); the manifest narrows it.
    #[test]
    fn no_network_narrows_when_only_the_manifest_asks_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "no_network: true\n");

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        apply(&mut args).expect("applies");
        assert!(args.no_network);
    }

    /// Stricter-wins, CLI direction: the CLI already narrowed
    /// `--no-network`; the manifest leaving it unstated must never widen
    /// the delegation back open.
    #[test]
    fn no_network_stays_narrowed_when_only_the_cli_asked_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "brief: go\n");

        let mut args = args_for("claude", "");
        args.manifest = Some(manifest_path);
        args.no_network = true;
        apply(&mut args).expect("applies");
        assert!(args.no_network);
    }

    /// Stricter-wins, manifest direction: a tighter manifest ceiling wins
    /// over a looser (or absent) CLI one.
    #[test]
    fn budget_tokens_takes_the_manifests_tighter_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "budget_tokens: 100\n");

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        args.budget_tokens = Some(1000);
        apply(&mut args).expect("applies");
        assert_eq!(args.budget_tokens, Some(100));
    }

    /// Stricter-wins, CLI direction: a tighter CLI ceiling wins over a
    /// looser manifest one -- the manifest can never widen it back up.
    #[test]
    fn budget_tokens_keeps_the_clis_tighter_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "budget_tokens: 1000\n");

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        args.budget_tokens = Some(100);
        apply(&mut args).expect("applies");
        assert_eq!(args.budget_tokens, Some(100));
    }

    #[test]
    fn max_tool_calls_takes_the_stricter_ceiling_regardless_of_source() {
        assert_eq!(stricter_ceiling(Some(10u32), Some(5u32)), Some(5));
        assert_eq!(stricter_ceiling(Some(5u32), Some(10u32)), Some(5));
        assert_eq!(stricter_ceiling::<u32>(None, Some(5)), Some(5));
        assert_eq!(stricter_ceiling::<u32>(Some(5), None), Some(5));
    }

    /// `mode`: a manifest asking for `read-only` narrows a `writing` CLI
    /// (or default); a manifest asking for `writing` never widens a CLI
    /// that already narrowed to `read-only`.
    #[test]
    fn read_only_mode_wins_regardless_of_source() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_narrows = tmp.path().join("narrow.yaml");
        write(&manifest_narrows, "mode: read-only\n");
        let mut from_manifest = args_for("claude", "go");
        from_manifest.manifest = Some(manifest_narrows);
        apply(&mut from_manifest).expect("applies");
        assert_eq!(from_manifest.mode, WorkerMode::ReadOnly);

        let manifest_writing = tmp.path().join("writing.yaml");
        write(&manifest_writing, "mode: writing\n");
        let mut from_cli = args_for("claude", "go");
        from_cli.manifest = Some(manifest_writing);
        from_cli.mode = WorkerMode::ReadOnly;
        apply(&mut from_cli).expect("applies");
        assert_eq!(from_cli.mode, WorkerMode::ReadOnly);
    }

    /// `path_scope`: when only one side narrows, that side wins outright;
    /// when both narrow to genuinely different sets, the intersection is
    /// the strictest defensible outcome.
    #[test]
    fn path_scope_narrows_to_the_intersection_when_both_sides_disagree() {
        let cli = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let manifest = vec![PathBuf::from("/repo/b"), PathBuf::from("/repo/c")];
        assert_eq!(
            stricter_path_scope(cli, manifest),
            vec![PathBuf::from("/repo/b")]
        );
        assert_eq!(
            stricter_path_scope(Vec::new(), vec![PathBuf::from("/repo/a")]),
            vec![PathBuf::from("/repo/a")]
        );
        assert_eq!(
            stricter_path_scope(vec![PathBuf::from("/repo/a")], Vec::new()),
            vec![PathBuf::from("/repo/a")]
        );
    }

    #[test]
    fn a_manifest_over_the_size_cap_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("huge.yaml");
        let oversized = format!("brief: {}\n", "a".repeat(MAX_MANIFEST_BYTES + 1));
        write(&manifest_path, &oversized);

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        let error = apply(&mut args).expect_err("must refuse an oversized manifest");
        assert!(error.to_string().contains("limit is"), "got {error}");
    }

    /// `result: {schema}` and `result: {kind}` are mutually exclusive
    /// inside the manifest itself, mirroring `--result-schema`/
    /// `--result-kind`'s own `conflicts_with`.
    #[test]
    fn manifest_result_schema_and_kind_are_mutually_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(
            &manifest_path,
            "result:\n  schema: schema.json\n  kind: review\n",
        );

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        let error = apply(&mut args).expect_err("must refuse both at once");
        assert!(
            error.to_string().contains("mutually exclusive"),
            "got {error}"
        );
    }

    /// A CLI `--result-kind` and a manifest `result.schema` are two
    /// different contract shapes -- always a conflict, never a silent
    /// pick, even though they are spelled as different `AgentArgs` fields.
    #[test]
    fn a_manifest_result_schema_conflicts_with_a_cli_result_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("m.yaml");
        write(&manifest_path, "result:\n  schema: schema.json\n");

        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest_path);
        args.result_kind = Some("review".to_string());
        let error = apply(&mut args).expect_err("must refuse the cross-field conflict");
        assert!(error.to_string().contains("review"), "got {error}");
    }
}
