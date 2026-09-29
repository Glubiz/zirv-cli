//! The native memory/context tools (issues #474-#475, roadmap N05-N06):
//! `memory_recall`, `memory_remember`, `memory_forget` and `context_search`.
//! They use the CLI's memory/context services so both surfaces agree.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolError, ToolErrorCode};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::state;

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MemoryToolScope {
    Private,
    Global,
    Shared,
    #[default]
    Session,
}

impl MemoryToolScope {
    fn memory_scope(self) -> crate::commands::ctx::memory::MemoryScope {
        use crate::commands::ctx::memory::MemoryScope;
        match self {
            Self::Private => MemoryScope::Private,
            Self::Global => MemoryScope::Global,
            Self::Shared => MemoryScope::Shared,
            Self::Session => MemoryScope::Session,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Global => "global",
            Self::Shared => "shared",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryRecallArgs {
    #[serde(default)]
    pub(super) key: Option<String>,
    #[serde(default)]
    pub(super) scope: Option<MemoryToolScope>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryRememberArgs {
    pub(super) key: String,
    pub(super) text: String,
    #[serde(default)]
    pub(super) scope: MemoryToolScope,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryForgetArgs {
    pub(super) key: String,
    #[serde(default)]
    pub(super) scope: MemoryToolScope,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextSearchArgs {
    pub(super) query: String,
}

impl super::NativeToolClient {
    pub(super) fn recall_memory(&self, args: MemoryRecallArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::memory::{self, MemoryScope};

        let cfg = CtxConfig::load(&self.repo, &|key| std::env::var(key).ok())
            .map_err(ToolError::external)?;
        let slug = state::repo_slug(&self.repo);
        let scopes: Vec<MemoryToolScope> = args.scope.map_or_else(
            || {
                vec![
                    MemoryToolScope::Session,
                    MemoryToolScope::Private,
                    MemoryToolScope::Global,
                    MemoryToolScope::Shared,
                ]
            },
            |scope| vec![scope],
        );
        let mut rows = Vec::new();
        for scope in scopes {
            let entries = match scope.memory_scope() {
                MemoryScope::Session if cfg.memory.session_enabled => {
                    memory::list_session(&self.state, &slug, &self.broker.identity().session)
                }
                MemoryScope::Session => Ok(Vec::new()),
                scope => memory::list_scoped(scope, &self.repo, &self.state, &slug, &cfg),
            }
            .map_err(ToolError::external)?;
            for (_, entry) in entries {
                if args.key.as_deref().is_none_or(|key| key == entry.key) {
                    rows.push(json!({"scope": scope.label(), "entry": entry}));
                }
            }
        }
        Ok(json!({"entries": rows}))
    }

    pub(super) fn remember_memory(&self, args: MemoryRememberArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::memory::{self, Entry, MemoryScope};

        let cfg = CtxConfig::load(&self.repo, &|key| std::env::var(key).ok())
            .map_err(ToolError::external)?;
        let scope = args.scope.memory_scope();
        if !scope.enabled(&cfg) {
            return Err(ToolError::new(
                ToolErrorCode::AuthorizationDenied,
                format!("memory write disabled by {}", scope.disabled_reason(&cfg)),
            ));
        }
        let slug = state::repo_slug(&self.repo);
        let now = state::now_secs();
        let entry = Entry {
            key: args.key,
            written_by: format!("native:{}", self.broker.identity().short),
            written: now,
            verified: now,
            source: "explicit".into(),
            body: args.text,
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        match scope {
            MemoryScope::Session => memory::remember_session(
                &self.state,
                &slug,
                &self.broker.identity().session,
                &entry,
                &cfg,
            ),
            scope => memory::upsert_scoped(scope, &self.repo, &self.state, &slug, &cfg, &entry),
        }
        .map_err(ToolError::external)?;
        Ok(json!({
            "stored": true,
            "scope": args.scope.label(),
            "key": entry.key,
        }))
    }

    pub(super) fn forget_memory(&self, args: MemoryForgetArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::memory::{self, MemoryScope};

        let slug = state::repo_slug(&self.repo);
        let removed = match args.scope.memory_scope() {
            MemoryScope::Session => memory::forget_session(
                &self.state,
                &slug,
                &self.broker.identity().session,
                &args.key,
            )
            .map_err(ToolError::external)?,
            scope => {
                memory::forget_scoped(scope, &self.repo, &self.state, &slug, &args.key)
                    .map_err(ToolError::external)?
                    .removed
            }
        };
        Ok(json!({
            "removed": removed,
            "scope": args.scope.label(),
            "key": args.key,
        }))
    }

    pub(super) fn search_context(&self, args: ContextSearchArgs) -> Result<Value, ToolError> {
        let state_root = self.state.root().to_string_lossy().into_owned();
        let env = |key: &str| {
            if key == state::STATE_ENV {
                Some(state_root.clone())
            } else {
                std::env::var(key).ok()
            }
        };
        let mut output = Vec::new();
        let code = crate::commands::ctx::search::run_with(
            &crate::commands::ctx::search::SearchArgs {
                query: Some(args.query),
                around: None,
                session: None,
                all_repos: false,
                json: true,
            },
            &mut output,
            &self.repo,
            &env,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        if code != 0 {
            return Err(ToolError::new(
                ToolErrorCode::Internal,
                format!("context search exited with {code}"),
            ));
        }
        serde_json::from_slice(output.trim_ascii())
            .map_err(|error| ToolError::new(ToolErrorCode::Internal, error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        CONTEXT_SEARCH, ExecutionAction, MEMORY_FORGET, MEMORY_RECALL, MEMORY_REMEMBER,
        ToolRegistry,
    };
    use super::*;

    #[test]
    fn knowledge_tools_have_typed_scope_and_effects() {
        let registry = ToolRegistry::native();
        for name in [
            MEMORY_RECALL,
            MEMORY_REMEMBER,
            MEMORY_FORGET,
            CONTEXT_SEARCH,
        ] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }
        let parsed = registry
            .parse(
                MEMORY_REMEMBER,
                json!({"key":"architecture", "text":"native", "scope":"shared"}),
            )
            .expect("parse memory write");
        assert_eq!(
            parsed.action().expect("action"),
            ExecutionAction::Knowledge {
                service: "memory".into(),
                operation: "remember".into(),
                scope: Some("shared".into()),
                key: Some("architecture".into()),
                write: true,
            }
        );
    }
}
