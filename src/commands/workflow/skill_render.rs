//! Issue #539's shared plain-text skill rendering.
//!
//! Exactly one place turns a [`SkillDigest`]/[`RegisteredSkill`] into the
//! text an operator reads, mirroring the precedent set by
//! `engine::write_registry_list`/`write_registry_entry` (issue #542 chunk
//! 3b, decision 5): the headless `zirv skill` CLI and the native
//! `/skills`/`/skill <id>` slash commands must never be able to print two
//! different tables for the same registry, so both funnel through these two
//! functions instead of formatting their own.

use std::io::Write;

use super::skill::{RegisteredSkill, SkillDigest, SkillSource};
use crate::commands::ctx::CtxResult;

/// `true` only for [`SkillSource::Repository`] -- the one layer a checkout
/// controls and that `SkillRegistry::load_for_repo` therefore treats as
/// untrusted input (mirrors the `SourceTrust::RepositoryUntrusted` split
/// `runtime::context::append_workflow_sources` applies to the same source
/// tag). Built-in and operator-global skills are both trusted: an operator
/// controls their own `~/.zirv/skills`, a checkout does not.
fn is_untrusted(source: SkillSource) -> bool {
    matches!(source, SkillSource::Repository)
}

fn joined<T: std::fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The plain-text table `/skills` and any future `zirv skill list` rewrite
/// print: one line per resolved skill, tab-separated so a terminal or a
/// script can split on it without a parser.
pub fn write_digest_list(writer: &mut impl Write, digests: &[SkillDigest<'_>]) -> CtxResult<()> {
    writeln!(
        writer,
        "ID\tVERSION\tSOURCE\tHASH\tPHASES\tCAPS\tINTEGRATIONS\tWRITES\tDESCRIPTION"
    )?;
    for digest in digests {
        let hash_prefix = &digest.content_hash[..digest.content_hash.len().min(12)];
        writeln!(
            writer,
            "{}\t{}\t{}\t{hash_prefix}\t{}\t{}\t{}\t{}\t{}",
            digest.id,
            digest.version,
            digest.source,
            joined(digest.phases),
            joined(digest.required_capabilities),
            joined(digest.required_integrations),
            if digest.external_writes { "rw" } else { "ro" },
            digest.description,
        )?;
    }
    Ok(())
}

/// The plain-text detail `/skill <id>` and any future `zirv skill show`
/// rewrite print for one resolved skill: everything a caller needs to
/// decide whether to trust and activate it, but never the instruction
/// body -- that is a separate disclosure stage the caller opts into
/// explicitly (issue #539's progressive disclosure), never bundled in here.
pub fn write_digest_detail(writer: &mut impl Write, skill: &RegisteredSkill) -> CtxResult<()> {
    let digest = skill.digest();
    writeln!(writer, "{}@{}", digest.id, digest.version)?;
    writeln!(writer, "name: {}", digest.name)?;
    writeln!(writer, "description: {}", digest.description)?;
    writeln!(writer, "source: {}", digest.source)?;
    writeln!(
        writer,
        "trust: {}",
        if is_untrusted(digest.source) {
            "untrusted"
        } else {
            "trusted"
        }
    )?;
    if let Some(path) = &skill.source_path {
        writeln!(writer, "path: {}", path.display())?;
    }
    writeln!(writer, "content-hash: {}", digest.content_hash)?;
    writeln!(
        writer,
        "budget: {} bytes ({} instruction bytes used)",
        skill.manifest.context_budget_bytes, digest.instruction_bytes
    )?;
    writeln!(writer, "phases: {}", joined(digest.phases))?;
    writeln!(
        writer,
        "capabilities: {}",
        joined(digest.required_capabilities)
    )?;
    writeln!(
        writer,
        "integrations: {}",
        joined(digest.required_integrations)
    )?;
    writeln!(
        writer,
        "writes: {}",
        if digest.external_writes {
            "rw (mutates an external service)"
        } else {
            "ro (investigation-only)"
        }
    )?;
    writeln!(
        writer,
        "implicit-activation: {}",
        if digest.implicit_activation {
            "yes (eligible for automatic activation)"
        } else {
            "no (explicit invocation only)"
        }
    )?;
    if digest.resource_count == 0 {
        writeln!(writer, "resources: (none)")?;
    } else {
        writeln!(writer, "resources:")?;
        for resource in &skill.resources {
            writeln!(
                writer,
                "  {}\t{}\t{} B",
                resource.path, resource.kind, resource.bytes
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::skill::SkillRegistry;

    /// A fixture registry of just the built-ins -- large and varied enough
    /// (24 manifests as of issue #539) to exercise every column without a
    /// hand-rolled manifest, and immune to drift from any one skill's own
    /// wording the way a hard-coded fixture id would be.
    fn fixture_registry() -> SkillRegistry {
        let repo = tempfile::tempdir().expect("tempdir");
        SkillRegistry::load(repo.path(), None, false, false).expect("built-in registry")
    }

    #[test]
    fn digest_list_has_a_header_and_one_tab_separated_line_per_skill() {
        let registry = fixture_registry();
        let digests = registry.digests();
        let mut buf = Vec::new();
        write_digest_list(&mut buf, &digests).expect("render list");
        let text = String::from_utf8(buf).expect("utf8");
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("ID\tVERSION\tSOURCE\tHASH\tPHASES\tCAPS\tINTEGRATIONS\tWRITES\tDESCRIPTION")
        );
        assert_eq!(lines.count(), digests.len());
        assert!(text.contains("built-in"));
    }

    #[test]
    fn digest_list_truncates_the_hash_to_twelve_characters() {
        let registry = fixture_registry();
        let digests = registry.digests();
        let mut buf = Vec::new();
        write_digest_list(&mut buf, &digests).expect("render list");
        let text = String::from_utf8(buf).expect("utf8");
        for digest in &digests {
            let hash_prefix = &digest.content_hash[..digest.content_hash.len().min(12)];
            assert!(
                text.contains(hash_prefix),
                "expected truncated hash {hash_prefix} for {}",
                digest.id
            );
            // The untruncated hash must never leak into the discovery table.
            if digest.content_hash.len() > 12 {
                assert!(!text.contains(digest.content_hash));
            }
        }
    }

    #[test]
    fn digest_detail_names_source_trust_hash_and_resources_but_not_the_body() {
        let registry = fixture_registry();
        let skill = registry.get("implement").expect("implement skill");
        let mut buf = Vec::new();
        write_digest_detail(&mut buf, skill).expect("render detail");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.starts_with("implement@"));
        assert!(text.contains("source: built-in"));
        assert!(text.contains("trust: trusted"));
        assert!(text.contains(&format!("content-hash: {}", skill.content_hash)));
        assert!(text.contains("writes: ro"));
        assert!(text.contains("implicit-activation: yes"));
        assert!(
            !text.contains(skill.manifest.instructions.as_str()),
            "the instruction body is a separate disclosure stage"
        );
    }

    #[test]
    fn digest_detail_marks_a_repository_skill_untrusted() {
        let repo = tempfile::tempdir().expect("tempdir");
        let dir = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("fixture.yaml"),
            "schema_version: 1\nid: fixture-repo-skill\nversion: 1\nname: Fixture\ndescription: repo fixture\ncontext_budget_bytes: 64\nphases: [implement]\ninstructions: do the thing\n",
        )
        .expect("write fixture skill");
        let registry =
            SkillRegistry::load(repo.path(), None, true, true).expect("registry with repo skill");
        let skill = registry.get("fixture-repo-skill").expect("fixture skill");
        let mut buf = Vec::new();
        write_digest_detail(&mut buf, skill).expect("render detail");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("source: repository-untrusted"));
        assert!(text.contains("trust: untrusted"));
    }
}
