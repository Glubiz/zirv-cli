//! Layered, trust-checked registry over [`WorkflowDefinitionV2`] packs:
//! zirv's own built-ins (compiled in via `include_str!`), optional
//! operator-global overrides under `~/.zirv/workflows/`, and optional
//! untrusted repository additions under `<repo>/.zirv/workflows/` -- mirrors
//! [`super::skill::SkillRegistry`] / [`super::agents::AgentRegistry`]'s own
//! three-layer trust model (issue #542, chunks 1+2).
//!
//! A repository-layer pack can never replace a trusted id (same rule as
//! skills/agents), and additionally can never WIDEN authority beyond a fixed
//! ceiling: `effects = "external"` is unconditionally refused for a
//! repository pack -- a categorical policy independent of built-in
//! precedent (issue #542 chunk 5 shipped the first built-ins that declare it,
//! `devops-infrastructure-change`/`sre-deploy-or-rollback`, and the refusal
//! did not become conditional on that; only zirv's own versioned built-ins
//! may ever reach for it). A watched capability (`repo.write`/`shell.exec`/
//! `network.access`/`agent.spawn`) no built-in step anywhere declares, or
//! dropping a gate category a same-domain built-in establishes, are each
//! refused too, but THOSE two checks stay relative to whatever the built-ins
//! currently exercise. Every refusal here is warned (dropped), not loaded,
//! rather than hard-failing the whole untrusted layer -- the same
//! "collision is dropped with a warning" posture `skill::load_dir` already
//! uses for an id collision, extended to a widening attempt.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::capability::CapabilityId;
use super::definition::{EffectClass, WorkflowDefinitionV2};
use super::skill::SkillRegistry;
use crate::commands::ctx::CtxResult;

const MAX_MANIFEST_BYTES: usize = 32 * 1024;
const MAX_WORKFLOW_DIRECTORY_ENTRIES: usize = 512;

/// Capabilities the widening refusal polices (issue #542 architecture §2):
/// a repository pack may only declare one of these on a step when some
/// built-in pack already declares it on one of its own steps, anywhere.
const WATCHED_CAPABILITIES: [CapabilityId; 4] = [
    CapabilityId::RepoWrite,
    CapabilityId::ShellExec,
    CapabilityId::NetworkAccess,
    CapabilityId::AgentSpawn,
];

/// `(id, embedded pack text)` for every built-in pack. Order here is
/// cosmetic (the registry keys on `id`); this only needs to name each file
/// once for `include_str!` and for the "file id matches declared id" check.
fn builtin_sources() -> [(&'static str, &'static str); 32] {
    [
        ("feature", include_str!("packs/feature.toml")),
        ("bugfix", include_str!("packs/bugfix.toml")),
        ("refactor", include_str!("packs/refactor.toml")),
        ("spike", include_str!("packs/spike.toml")),
        ("review", include_str!("packs/review.toml")),
        ("adaptive-work", include_str!("packs/adaptive-work.toml")),
        // Issue #542 chunk 4: first-wave professional packs, one per group.
        (
            "pm-requirements",
            include_str!("packs/pm-requirements.toml"),
        ),
        (
            "pm-status-report",
            include_str!("packs/pm-status-report.toml"),
        ),
        (
            "data-question-to-report",
            include_str!("packs/data-question-to-report.toml"),
        ),
        (
            "data-quality-investigation",
            include_str!("packs/data-quality-investigation.toml"),
        ),
        (
            "architecture-decision-record",
            include_str!("packs/architecture-decision-record.toml"),
        ),
        (
            "architecture-design-review",
            include_str!("packs/architecture-design-review.toml"),
        ),
        (
            "sre-incident-triage",
            include_str!("packs/sre-incident-triage.toml"),
        ),
        (
            "devops-ci-cd-change",
            include_str!("packs/devops-ci-cd-change.toml"),
        ),
        (
            "dependency-upgrade",
            include_str!("packs/dependency-upgrade.toml"),
        ),
        (
            "security-remediation",
            include_str!("packs/security-remediation.toml"),
        ),
        // Issue #542 chunk 5: the remaining catalogue.
        (
            "pm-backlog-triage",
            include_str!("packs/pm-backlog-triage.toml"),
        ),
        (
            "pm-cycle-planning",
            include_str!("packs/pm-cycle-planning.toml"),
        ),
        ("pm-risk-review", include_str!("packs/pm-risk-review.toml")),
        (
            "pm-retrospective",
            include_str!("packs/pm-retrospective.toml"),
        ),
        (
            "data-anomaly-investigation",
            include_str!("packs/data-anomaly-investigation.toml"),
        ),
        (
            "data-recurring-kpi-review",
            include_str!("packs/data-recurring-kpi-review.toml"),
        ),
        (
            "architecture-discovery",
            include_str!("packs/architecture-discovery.toml"),
        ),
        (
            "architecture-migration-roadmap",
            include_str!("packs/architecture-migration-roadmap.toml"),
        ),
        (
            "architecture-threat-scale-cost-review",
            include_str!("packs/architecture-threat-scale-cost-review.toml"),
        ),
        (
            "devops-infrastructure-change",
            include_str!("packs/devops-infrastructure-change.toml"),
        ),
        (
            "sre-deploy-or-rollback",
            include_str!("packs/sre-deploy-or-rollback.toml"),
        ),
        ("sre-postmortem", include_str!("packs/sre-postmortem.toml")),
        (
            "sre-capacity-reliability-review",
            include_str!("packs/sre-capacity-reliability-review.toml"),
        ),
        (
            "schema-data-migration",
            include_str!("packs/schema-data-migration.toml"),
        ),
        (
            "performance-investigation",
            include_str!("packs/performance-investigation.toml"),
        ),
        (
            "documentation-runbook-change",
            include_str!("packs/documentation-runbook-change.toml"),
        ),
    ]
}

/// Parses ONE embedded built-in pack by id, without constructing a whole
/// registry (no skill-registry cross-check, no disk I/O) -- issue #542
/// chunk 3a: `engine::WorkflowState::start`'s fallback when a live registry
/// lookup is unavailable or does not (yet) know this id. Built-in pack text
/// is compiled into the binary and proven to parse by `every_builtin_pack_
/// parses_and_validates`, so a parse failure here is a build-time invariant
/// violation, not a runtime condition callers need to handle.
pub(crate) fn builtin_definition(id: &str) -> Option<WorkflowDefinitionV2> {
    let (_, text) = builtin_sources()
        .into_iter()
        .find(|(pack_id, _)| *pack_id == id)?;
    Some(
        toml::from_str(text)
            .unwrap_or_else(|err| panic!("embedded built-in pack '{id}' failed to parse: {err}")),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowSource {
    BuiltIn,
    OperatorGlobal,
    Repository,
}

impl std::fmt::Display for WorkflowSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BuiltIn => "built-in",
            Self::OperatorGlobal => "operator-global",
            Self::Repository => "repository-untrusted",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredWorkflow {
    #[serde(flatten)]
    pub definition: WorkflowDefinitionV2,
    pub source: WorkflowSource,
    pub source_path: Option<PathBuf>,
    /// [`WorkflowDefinitionV2::hash`], computed once at load time so every
    /// reader (list/show, and `engine::WorkflowState::start`'s pinning) uses
    /// the identical value rather than re-hashing.
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct WorkflowRegistry {
    workflows: BTreeMap<String, RegisteredWorkflow>,
    warnings: Vec<String>,
}

impl WorkflowRegistry {
    /// `include_repo` is the operator's `workflow.repo_workflows_enabled`;
    /// see [`Self::load_for_repo`]. `skills` supplies the known skill ids
    /// every pack's steps are validated against -- the caller's own already-
    /// loaded [`SkillRegistry`], so a pack cannot reference a skill this
    /// session would itself refuse to resolve.
    pub fn load(
        repo: &Path,
        home: Option<&Path>,
        include_custom: bool,
        include_repo: bool,
        skills: &SkillRegistry,
    ) -> CtxResult<Self> {
        let known_skill_ids: BTreeSet<&str> = skills
            .list()
            .map(|skill| skill.manifest.id.as_str())
            .collect();
        let mut workflows = BTreeMap::new();
        let mut warnings = Vec::new();

        for (file_id, text) in builtin_sources() {
            let definition: WorkflowDefinitionV2 = toml::from_str(text).map_err(|err| {
                format!("built-in workflow pack '{file_id}.toml' failed to parse: {err}")
            })?;
            if definition.id != file_id {
                return Err(format!(
                    "built-in workflow pack file '{file_id}.toml' declares id '{}'",
                    definition.id
                )
                .into());
            }
            definition
                .validate(&known_skill_ids)
                .map_err(|err| format!("built-in workflow pack '{file_id}': {err}"))?;
            let hash = definition.hash()?;
            workflows.insert(
                definition.id.clone(),
                RegisteredWorkflow {
                    definition,
                    source: WorkflowSource::BuiltIn,
                    source_path: None,
                    hash,
                },
            );
        }

        if include_custom {
            if let Some(home) = home {
                load_dir(
                    &home.join(".zirv").join("workflows"),
                    home,
                    WorkflowSource::OperatorGlobal,
                    &known_skill_ids,
                    &mut workflows,
                    &mut warnings,
                )?;
            }
            if include_repo {
                load_dir(
                    &repo.join(".zirv").join("workflows"),
                    repo,
                    WorkflowSource::Repository,
                    &known_skill_ids,
                    &mut workflows,
                    &mut warnings,
                )?;
            }
        }

        Ok(Self {
            workflows,
            warnings,
        })
    }

    /// [`Self::load`] with `include_repo` taken from the operator's
    /// `[workflow] repo_workflows_enabled` (`REPO_FORBIDDEN`, off by
    /// default -- see `config.rs`). A configuration that will not even
    /// parse closes the gate, same fail-closed posture as
    /// `SkillRegistry::load_for_repo`/`AgentRegistry::load_for_repo`.
    pub fn load_for_repo(
        repo: &Path,
        home: Option<&Path>,
        include_custom: bool,
        skills: &SkillRegistry,
    ) -> CtxResult<Self> {
        Self::load(
            repo,
            home,
            include_custom,
            super::repo_gates(repo).workflows,
            skills,
        )
    }

    pub fn list(&self) -> impl Iterator<Item = &RegisteredWorkflow> {
        self.workflows.values()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn get(&self, id: &str) -> CtxResult<&RegisteredWorkflow> {
        self.workflows
            .get(id)
            .ok_or_else(|| format!("unknown workflow '{id}'").into())
    }
}

/// Loads every `.toml`/`.yaml`/`.yml` manifest directly under `root` (no
/// recursion), applying the same symlink/path-escape/size/entry-count
/// defenses `skill::load_dir` and `agents::load_dir` already apply to their
/// own repo-owned surfaces, then this module's own trust rules for `source`.
fn load_dir(
    root: &Path,
    allowed_root: &Path,
    source: WorkflowSource,
    known_skill_ids: &BTreeSet<&str>,
    workflows: &mut BTreeMap<String, RegisteredWorkflow>,
    warnings: &mut Vec<String>,
) -> CtxResult<()> {
    if !root.exists() {
        return Ok(());
    }
    let root_metadata = std::fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() {
        return Err(format!("refusing symlinked workflow directory '{}'", root.display()).into());
    }
    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "cannot resolve workflow directory '{}': {err}",
            root.display()
        )
    })?;
    let canonical_allowed_root = allowed_root.canonicalize().map_err(|err| {
        format!(
            "cannot resolve workflow trust root '{}': {err}",
            allowed_root.display()
        )
    })?;
    if !canonical_root.starts_with(&canonical_allowed_root) {
        return Err(format!(
            "workflow directory '{}' escapes trust root '{}'",
            root.display(),
            allowed_root.display()
        )
        .into());
    }
    if !canonical_root.is_dir() {
        return Err(format!("workflow path '{}' is not a directory", root.display()).into());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&canonical_root)? {
        if entries.len() == MAX_WORKFLOW_DIRECTORY_ENTRIES {
            return Err(format!(
                "workflow directory '{}' has more than {MAX_WORKFLOW_DIRECTORY_ENTRIES} entries",
                root.display()
            )
            .into());
        }
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(
                format!("refusing symlinked workflow manifest '{}'", path.display()).into(),
            );
        }
        if !metadata.is_file() {
            continue;
        }
        let extension = path.extension().and_then(|value| value.to_str());
        if !matches!(extension, Some("yaml" | "yml" | "toml")) {
            continue;
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&canonical_root) {
            return Err(format!(
                "workflow manifest escapes '{}': {}",
                root.display(),
                path.display()
            )
            .into());
        }
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if size > MAX_MANIFEST_BYTES {
            return Err(format!(
                "workflow manifest '{}' is {size} bytes; limit is {MAX_MANIFEST_BYTES}",
                path.display()
            )
            .into());
        }
        let text = std::fs::read_to_string(&canonical)?;
        let definition: WorkflowDefinitionV2 = match extension {
            Some("toml") => toml::from_str(&text)
                .map_err(|err| format!("invalid workflow manifest '{}': {err}", path.display()))?,
            _ => serde_yaml_ng::from_str(&text)
                .map_err(|err| format!("invalid workflow manifest '{}': {err}", path.display()))?,
        };
        definition
            .validate(known_skill_ids)
            .map_err(|err| format!("invalid workflow manifest '{}': {err}", path.display()))?;

        match source {
            WorkflowSource::BuiltIn => unreachable!("built-ins never load through load_dir"),
            WorkflowSource::OperatorGlobal => {
                let replacing_builtin = workflows
                    .get(&definition.id)
                    .is_some_and(|existing| existing.source == WorkflowSource::BuiltIn);
                if replacing_builtin && !definition.override_builtin {
                    warnings.push(format!(
                        "operator-global workflow '{}' ({}) is ignored: set override = true to \
                         replace the built-in pack",
                        definition.id,
                        path.display()
                    ));
                    continue;
                }
                let hash = definition.hash()?;
                workflows.insert(
                    definition.id.clone(),
                    RegisteredWorkflow {
                        definition,
                        source,
                        source_path: Some(path),
                        hash,
                    },
                );
            }
            WorkflowSource::Repository => {
                if let Some(existing) = workflows.get(&definition.id) {
                    warnings.push(format!(
                        "repository workflow '{}' ({}) is ignored: id already provided by {}",
                        definition.id,
                        path.display(),
                        existing.source
                    ));
                    continue;
                }
                if let Some(reason) = widening_violation(&definition, workflows) {
                    warnings.push(format!(
                        "repository workflow '{}' ({}) is ignored: {reason}",
                        definition.id,
                        path.display()
                    ));
                    continue;
                }
                let hash = definition.hash()?;
                workflows.insert(
                    definition.id.clone(),
                    RegisteredWorkflow {
                        definition,
                        source,
                        source_path: Some(path),
                        hash,
                    },
                );
            }
        }
    }
    Ok(())
}

/// `Some(reason)` when `candidate` (an untrusted repository pack) would
/// widen authority beyond every already-registered built-in pack; `None`
/// when it is safe to add. See this module's own doc comment for the exact
/// three checks.
fn widening_violation(
    candidate: &WorkflowDefinitionV2,
    registered: &BTreeMap<String, RegisteredWorkflow>,
) -> Option<String> {
    if candidate.effects == EffectClass::External {
        return Some(
            "effects = \"external\" widens repository authority beyond any built-in pack".into(),
        );
    }

    let builtin_capabilities: BTreeSet<CapabilityId> = registered
        .values()
        .filter(|workflow| workflow.source == WorkflowSource::BuiltIn)
        .flat_map(|workflow| {
            workflow
                .definition
                .steps
                .iter()
                .flat_map(|step| step.capabilities.iter().copied())
        })
        .filter(|capability| WATCHED_CAPABILITIES.contains(capability))
        .collect();
    for step in &candidate.steps {
        for capability in &step.capabilities {
            if WATCHED_CAPABILITIES.contains(capability)
                && !builtin_capabilities.contains(capability)
            {
                return Some(format!(
                    "step '{}' requires capability '{capability}', which no built-in pack uses",
                    step.id
                ));
            }
        }
    }

    type GateFloor = fn(&super::definition::GateSpec) -> bool;
    let floors: [(&str, GateFloor); 3] = [
        ("approval", |gates| !gates.approval.is_empty()),
        ("validation", |gates| !gates.validation.is_empty()),
        ("independent-review", |gates| {
            !gates.independent_review.is_empty()
        }),
    ];
    for builtin in registered
        .values()
        .filter(|workflow| workflow.source == WorkflowSource::BuiltIn)
        .filter(|workflow| {
            workflow
                .definition
                .domains
                .iter()
                .any(|domain| candidate.domains.contains(domain))
        })
    {
        for (label, has_floor) in floors {
            if has_floor(&builtin.definition.gates) && !has_floor(&candidate.gates) {
                return Some(format!(
                    "removes the {label} gate built-in pack '{}' establishes for domain(s) {:?}",
                    builtin.definition.id, builtin.definition.domains
                ));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use tempfile::tempdir;

    fn skills() -> SkillRegistry {
        let repo = tempdir().unwrap();
        SkillRegistry::load(repo.path(), None, false, false).unwrap()
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write fixture");
    }

    /// `extra_root_keys` is inserted while still inside the root table --
    /// BEFORE `[[steps]]` opens the first subtable -- so a caller can add
    /// scalar overrides (`override = true`, a different `effects`) without
    /// producing invalid TOML (a bare key after a table header belongs to
    /// that table, not the root).
    fn minimal_pack(id: &str, extra_root_keys: &str) -> String {
        format!(
            r#"
schema_version = 1
id = "{id}"
version = 1
title = "Custom {id}"
description = "A minimal custom workflow pack."
effects = "repository"
{extra_root_keys}

[[steps]]
id = "only"
title = "Only step"
phase = "implement"
skills = ["implement"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#
        )
    }

    #[test]
    fn every_builtin_pack_parses_and_validates() {
        let registry = WorkflowRegistry::load(Path::new("."), None, false, false, &skills())
            .expect("every built-in pack must load");
        let ids: BTreeSet<&str> = registry.list().map(|w| w.definition.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "adaptive-work",
                "architecture-decision-record",
                "architecture-design-review",
                "architecture-discovery",
                "architecture-migration-roadmap",
                "architecture-threat-scale-cost-review",
                "bugfix",
                "data-anomaly-investigation",
                "data-question-to-report",
                "data-quality-investigation",
                "data-recurring-kpi-review",
                "dependency-upgrade",
                "devops-ci-cd-change",
                "devops-infrastructure-change",
                "documentation-runbook-change",
                "feature",
                "performance-investigation",
                "pm-backlog-triage",
                "pm-cycle-planning",
                "pm-requirements",
                "pm-retrospective",
                "pm-risk-review",
                "pm-status-report",
                "refactor",
                "review",
                "schema-data-migration",
                "security-remediation",
                "spike",
                "sre-capacity-reliability-review",
                "sre-deploy-or-rollback",
                "sre-incident-triage",
                "sre-postmortem",
            ]
            .into_iter()
            .collect()
        );
        for workflow in registry.list() {
            assert_eq!(workflow.source, WorkflowSource::BuiltIn);
            assert!(!workflow.hash.is_empty());
        }
        assert!(registry.warnings().is_empty());
    }

    /// Issue #542 chunk 4: every built-in pack's steps must resolve against
    /// a real skill id (already enforced by `validate()`, re-proven here as
    /// the cross-check the issue's own brief asks for) and a KNOWN agent
    /// role. Capabilities are a closed enum, so an unrecognized one is
    /// already rejected at parse time -- there is nothing further to check
    /// for those here.
    ///
    /// Roles: `implementer`/`reviewer` (already registered on this branch)
    /// plus the full #541 role roster this chunk's (and chunk 4's)
    /// professional packs draw from (`architect`, `data-analyst`,
    /// `debugger`, `devops-sre`, `doc-keeper`, `explorer`, `planner`,
    /// `researcher`, `tester`, `security-scanner`) -- #541 has not merged
    /// into this worktree, so `AgentRegistry` does not yet carry those
    /// manifests, and this is a fixed allowlist (the exact 12 built-in
    /// manifest ids `native/541`'s `agents.rs` registers, confirmed by
    /// reading that branch's worktree at chunk-5 time) rather than a live
    /// lookup. Issue #542 chunk 5: replace this with the live
    /// `AgentRegistry` roster once #541 has actually merged into a shared
    /// base (see the chunk-5 design note's reconciliation section) -- a
    /// rebase step, not a chunk-5 gap, since every role used across the
    /// whole catalogue is one the #541 roster itself defines.
    #[test]
    fn no_builtin_pack_references_an_unknown_skill_or_role() {
        const KNOWN_ROLES: &[&str] = &[
            "implementer",
            "reviewer",
            "doc-keeper",
            "security-scanner",
            "explorer",
            "researcher",
            "planner",
            "architect",
            "debugger",
            "tester",
            "data-analyst",
            "devops-sre",
        ];
        let skills = skills();
        let known_skill_ids: BTreeSet<&str> = skills
            .list()
            .map(|skill| skill.manifest.id.as_str())
            .collect();
        let registry = WorkflowRegistry::load(Path::new("."), None, false, false, &skills)
            .expect("every built-in pack must load");
        for workflow in registry.list() {
            for step in &workflow.definition.steps {
                for skill in &step.skills {
                    assert!(
                        known_skill_ids.contains(skill.as_str()),
                        "{}: step '{}' references unknown skill '{}'",
                        workflow.definition.id,
                        step.id,
                        skill
                    );
                }
                if let Some(role) = &step.agent_role {
                    assert!(
                        KNOWN_ROLES.contains(&role.as_str()),
                        "{}: step '{}' references unknown agent role '{}'",
                        workflow.definition.id,
                        step.id,
                        role
                    );
                }
            }
        }
    }

    #[test]
    fn an_operator_pack_overrides_a_builtin_only_with_override_true() {
        let home = tempdir().unwrap();
        let repo = tempdir().unwrap();
        let dir = home.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir.join("feature.toml"), &minimal_pack("feature", ""));

        let registry =
            WorkflowRegistry::load(repo.path(), Some(home.path()), true, true, &skills())
                .expect("load with no override should still succeed, just warn");
        let feature = registry.get("feature").unwrap();
        assert_eq!(feature.source, WorkflowSource::BuiltIn);
        assert_eq!(registry.warnings().len(), 1);
        assert!(registry.warnings()[0].contains("override = true"));

        write(
            &dir.join("feature.toml"),
            &minimal_pack("feature", "override = true"),
        );
        let registry =
            WorkflowRegistry::load(repo.path(), Some(home.path()), true, true, &skills())
                .expect("load with override should succeed");
        let feature = registry.get("feature").unwrap();
        assert_eq!(feature.source, WorkflowSource::OperatorGlobal);
        assert_eq!(feature.definition.title, "Custom feature");
    }

    #[test]
    fn a_repository_pack_cannot_shadow_a_builtin_or_widen_effects() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir.join("feature.toml"), &minimal_pack("feature", ""));
        write(
            &dir.join("external.toml"),
            r#"
schema_version = 1
id = "external"
version = 1
title = "Custom external"
description = "Tries to widen authority to an external effect."
effects = "external"

[[steps]]
id = "only"
title = "Only step"
phase = "implement"
skills = ["implement"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#,
        );

        let registry = WorkflowRegistry::load(repo.path(), None, true, true, &skills())
            .expect("a widening/shadowing pack is dropped, not a hard failure");
        assert!(
            registry.get("external").is_err(),
            "external pack must not load"
        );
        assert_eq!(
            registry.get("feature").unwrap().source,
            WorkflowSource::BuiltIn,
            "the repository copy must not shadow the built-in"
        );
        assert_eq!(registry.warnings().len(), 2, "{:?}", registry.warnings());
        assert!(
            registry
                .warnings()
                .iter()
                .any(|w| w.contains("id already provided"))
        );
        assert!(registry.warnings().iter().any(|w| w.contains("external")));
    }

    #[test]
    fn repository_packs_are_ignored_unless_enabled() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir.join("extra.toml"), &minimal_pack("extra", ""));

        let registry = WorkflowRegistry::load(repo.path(), None, true, false, &skills()).unwrap();
        assert!(registry.get("extra").is_err());
        assert!(registry.warnings().is_empty());
    }

    #[test]
    fn symlinks_path_escapes_and_oversized_packs_are_refused() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        let oversized = format!(
            "{}\n# {}",
            minimal_pack("oversized", ""),
            "x".repeat(MAX_MANIFEST_BYTES)
        );
        write(&dir.join("oversized.toml"), &oversized);
        let error = WorkflowRegistry::load(repo.path(), None, true, true, &skills())
            .expect_err("an oversized manifest must hard-fail the load");
        assert!(error.to_string().contains("bytes"), "{error}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::remove_file(dir.join("oversized.toml")).unwrap();
            let outside = tempdir().unwrap();
            write(
                &outside.path().join("outside.toml"),
                &minimal_pack("outside", ""),
            );
            symlink(
                outside.path().join("outside.toml"),
                dir.join("outside.toml"),
            )
            .unwrap();
            let error = WorkflowRegistry::load(repo.path(), None, true, true, &skills())
                .expect_err("a symlinked manifest must be refused");
            assert!(error.to_string().contains("symlinked"), "{error}");
        }
    }

    /// A "hostile" repository pack fixture: declares a dangerous capability
    /// no built-in pack ever uses, and separately drops the approval floor a
    /// same-domain built-in establishes. Both are refused independently.
    #[test]
    fn a_hostile_repository_definition_cannot_widen_capabilities_or_drop_gates() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir.join("hostile-capability.toml"),
            r#"
schema_version = 1
id = "hostile-capability"
version = 1
title = "Hostile capability"
description = "Tries to widen authority with an unused capability."
domains = ["software"]
effects = "repository"

[[steps]]
id = "only"
title = "Only step"
phase = "implement"
skills = ["implement"]
capabilities = ["network.access"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#,
        );
        write(
            &dir.join("hostile-gate.toml"),
            r#"
schema_version = 1
id = "hostile-gate"
version = 1
title = "Hostile gate"
description = "Tries to drop the software domain's approval floor."
domains = ["software"]
effects = "repository"

[[steps]]
id = "only"
title = "Only step"
phase = "implement"
skills = ["implement"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#,
        );

        let registry = WorkflowRegistry::load(repo.path(), None, true, true, &skills()).unwrap();
        assert!(registry.get("hostile-capability").is_err());
        assert!(registry.get("hostile-gate").is_err());
        assert_eq!(registry.warnings().len(), 2, "{:?}", registry.warnings());
        assert!(
            registry
                .warnings()
                .iter()
                .any(|w| w.contains("network.access"))
        );
        assert!(
            registry
                .warnings()
                .iter()
                .any(|w| w.contains("approval gate"))
        );
    }
}
