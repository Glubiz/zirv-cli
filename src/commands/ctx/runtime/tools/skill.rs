//! The native skill tools (issue #539 chunk E1): `skill_list`, `skill_load`,
//! `skill_read_resource`. Thin typed arguments over `workflow::skill_tools`,
//! the exact functions the read-only MCP bridge (`ctx::mcp`) also calls --
//! see that module's own `SkillListArgs`/`SkillLoadArgs`/`SkillReadResourceArgs`
//! for the mirrored arg shapes.

use serde::Deserialize;
use serde_json::{Value, json};

use super::ToolError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SkillListArgs {
    #[serde(default)]
    pub(super) query: Option<String>,
    #[serde(default)]
    pub(super) phase: Option<String>,
    #[serde(default)]
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SkillLoadArgs {
    /// A bare skill id, or `id@version` to pin an exact version.
    pub(super) id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SkillReadResourceArgs {
    pub(super) id: String,
    /// Bundle-relative resource path, e.g. `references/checklist.md`.
    pub(super) path: String,
}

impl super::NativeToolClient {
    fn skill_registry_for_tools(
        &self,
    ) -> Result<crate::commands::workflow::skill::SkillRegistry, ToolError> {
        use crate::commands::workflow::skill::SkillRegistry;

        let home = crate::utils::home_dir().ok();
        SkillRegistry::load_for_repo(&self.repo, home.as_deref(), true).map_err(ToolError::external)
    }

    pub(super) fn skill_list(&self, args: &SkillListArgs) -> Result<Value, ToolError> {
        use crate::commands::workflow::skill::WorkflowPhase;
        use crate::commands::workflow::skill_tools;

        let registry = self.skill_registry_for_tools()?;
        let phase = args.phase.as_deref().and_then(WorkflowPhase::parse);
        skill_tools::skill_list(&registry, args.query.as_deref(), phase, args.limit)
            .map_err(ToolError::external)
    }

    /// Checks the session's own capability report FIRST (`ensure_supported`,
    /// inside `skill_tools::skill_load`) -- a refusal is the registry's own
    /// text, unchanged. On success, records one best-effort activation-
    /// journal entry (issue #539 chunk E1); a refusal records nothing.
    pub(super) fn skill_load(&self, args: &SkillLoadArgs) -> Result<Value, ToolError> {
        use crate::commands::workflow::capability::CapabilityReport;
        use crate::commands::workflow::skill_tools::{self, SkillLoadSurface};

        let registry = self.skill_registry_for_tools()?;
        let report = CapabilityReport::for_repo("native", &self.repo)
            .map_err(ToolError::external)?
            .with_integrations(self.services.integrations.clone());
        let loaded =
            skill_tools::skill_load(&registry, &args.id, &report).map_err(ToolError::external)?;
        let _ = skill_tools::record_skill_activation(
            &self.state,
            &self.repo,
            &loaded,
            SkillLoadSurface::NativeTool,
        );
        serde_json::to_value(&loaded).map_err(ToolError::external)
    }

    pub(super) fn skill_read_resource(
        &self,
        args: &SkillReadResourceArgs,
    ) -> Result<Value, ToolError> {
        use crate::commands::workflow::skill_tools;

        let registry = self.skill_registry_for_tools()?;
        let content = skill_tools::skill_read_resource(&registry, &args.id, &args.path)
            .map_err(ToolError::external)?;
        Ok(json!({ "content": content }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::tools::tests::*;
    use crate::commands::ctx::runtime::tools::*;

    #[test]
    fn every_skill_tool_is_registered_with_a_closed_schema_and_read_scope() {
        let registry = ToolRegistry::native();
        for name in [SKILL_LIST, SKILL_LOAD, SKILL_READ_RESOURCE] {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(definition.input_schema["type"], "object");
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert_eq!(definition.capabilities, vec!["tool_access".to_string()]);
            assert_eq!(
                definition.resource_claims,
                vec![ResourceClaimKind::ReadRoot],
                "{name} must be a plain read"
            );
        }
    }

    #[test]
    fn skill_load_tool_returns_instructions_and_records_one_activation() {
        let mut fixture = delegation_fixture(0);
        let receipt = call(
            &mut fixture.client,
            SKILL_LOAD,
            json!({"id":"incident-investigation"}),
        );
        assert!(receipt.error.is_none(), "{receipt:?}");
        let result = result_of(&receipt).clone();
        assert_eq!(result["id"], "incident-investigation");
        assert_eq!(result["version"], 1);
        assert!(!result["instructions"].as_array().unwrap().is_empty());
        let hash = result["content_hash"]
            .as_str()
            .expect("content hash")
            .to_string();
        assert!(!hash.is_empty());

        let activations =
            crate::commands::workflow::telemetry::skill_activations(&fixture.state, &fixture.repo)
                .expect("activations");
        assert_eq!(activations.len(), 1);
        assert_eq!(
            activations[0].skill_content_hash.as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            activations[0].skill_id.as_deref(),
            Some("incident-investigation")
        );
        assert_eq!(activations[0].skill_surface.as_deref(), Some("native-tool"));
    }

    #[test]
    fn skill_load_tool_refuses_an_unavailable_integration_and_records_no_activation() {
        let mut fixture = delegation_fixture(0);
        let receipt = call(
            &mut fixture.client,
            SKILL_LOAD,
            json!({"id":"kibana-log-investigation"}),
        );
        let error = receipt
            .error
            .expect("refused: no kibana MCP server is configured");
        assert!(error.message.contains("kibana"), "{}", error.message);

        let activations =
            crate::commands::workflow::telemetry::skill_activations(&fixture.state, &fixture.repo)
                .expect("activations");
        assert!(activations.is_empty(), "a refusal must not be journalled");
    }

    #[test]
    fn skill_list_tool_lists_and_ranks() {
        let mut fixture = delegation_fixture(0);
        let all = call(&mut fixture.client, SKILL_LIST, json!({}));
        assert!(all.error.is_none(), "{all:?}");
        let all_result = result_of(&all);
        assert!(!all_result["skills"].as_array().unwrap().is_empty());
        assert!(!all_result.to_string().contains("Restoring service"));

        let ranked = call(
            &mut fixture.client,
            SKILL_LIST,
            json!({"query": "production outage, paging alert"}),
        );
        assert!(ranked.error.is_none(), "{ranked:?}");
        let ranked_result = result_of(&ranked);
        assert_eq!(ranked_result["skills"][0]["id"], "incident-investigation");
    }

    #[test]
    fn skill_read_resource_tool_refuses_a_path_escape() {
        let mut fixture = delegation_fixture(0);
        let receipt = call(
            &mut fixture.client,
            SKILL_READ_RESOURCE,
            json!({"id":"incident-investigation", "path":"../x"}),
        );
        assert!(receipt.error.is_some());
    }
}
