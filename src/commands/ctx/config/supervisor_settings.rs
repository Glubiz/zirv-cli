use super::*;

/// Operator-only supervisor (#835): a strong model whose binding rulings only narrow, from one
/// bounded read-only consult per trigger, delivered as mail. Off by default; repos cannot enable it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisorConfig {
    pub enabled: bool,
    /// Harness that runs the consult: `claude` or `codex`.
    pub harness: String,
    /// Explicit consult model; required while enabled, never inferred.
    pub model: String,
    /// Deprecated and ignored: consults are uncapped. Parsed so existing configs load.
    pub max_calls: u32,
    /// Byte cap on a ruling's reason.
    pub max_advice_bytes: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            harness: "codex".to_string(),
            model: String::new(),
            max_calls: 3,
            max_advice_bytes: 2048,
        }
    }
}

impl SupervisorConfig {
    /// A config error while enabled with an unusable harness or no explicit model.
    pub(super) fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !matches!(self.harness.as_str(), "claude" | "codex") {
            return Err(format!(
                "supervisor.harness must be claude or codex, got '{}'",
                self.harness
            ));
        }
        if self.model.trim().is_empty() {
            return Err("supervisor.model is required when supervisor.enabled is true".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_valid_without_a_model() {
        let cfg = SupervisorConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_advice_bytes, 2048);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn enabled_requires_an_explicit_model_and_a_known_harness() {
        let mut cfg = SupervisorConfig {
            enabled: true,
            ..SupervisorConfig::default()
        };
        assert!(cfg.validate().unwrap_err().contains("supervisor.model"));
        cfg.model = "gpt-6-astra".to_string();
        assert!(cfg.validate().is_ok());
        cfg.harness = "gemini".to_string();
        assert!(cfg.validate().unwrap_err().contains("supervisor.harness"));
    }

    #[test]
    fn a_repository_cannot_set_any_supervisor_key_but_the_environment_can() {
        let empty = std::collections::HashMap::<String, String>::new();
        for toml in [
            "[supervisor]\nenabled = true\n",
            "[supervisor]\nharness = \"claude\"\n",
            "[supervisor]\nmodel = \"x\"\n",
            "[supervisor]\nmax_calls = 9\n",
            "[supervisor]\nmax_advice_bytes = 9\n",
        ] {
            let home = tempfile::tempdir().expect("tempdir");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
            let repo = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
            std::fs::write(repo.path().join(".zirv/ctx.toml"), toml).expect("write");
            let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
                .expect_err("a repository must not set supervisor keys");
            assert!(is_repo_forbidden(err.as_ref()), "{toml}: {err}");
        }
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = std::collections::HashMap::from([
            (
                "ZIRV_CTX_SUPERVISOR_ENABLED".to_string(),
                "true".to_string(),
            ),
            (
                "ZIRV_CTX_SUPERVISOR_HARNESS".to_string(),
                "claude".to_string(),
            ),
            ("ZIRV_CTX_SUPERVISOR_MODEL".to_string(), "m".to_string()),
            (
                "ZIRV_CTX_SUPERVISOR_MAX_ADVICE_BYTES".to_string(),
                "512".to_string(),
            ),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(cfg.supervisor.enabled);
        assert_eq!(cfg.supervisor.harness, "claude");
        assert_eq!(cfg.supervisor.model, "m");
        assert_eq!(cfg.supervisor.max_advice_bytes, 512);
    }

    #[test]
    fn the_deprecated_max_calls_key_still_loads_from_the_operator_config() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[supervisor]\nmax_calls = 3\n",
        )
        .expect("write");
        let repo = tempfile::tempdir().expect("tempdir");
        let empty = std::collections::HashMap::<String, String>::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("load");
        assert!(!cfg.supervisor.enabled);
    }

    #[test]
    fn the_old_architect_names_are_gone() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[architect]\nenabled = true\n",
        )
        .expect("write");
        let empty = std::collections::HashMap::<String, String>::new();
        let err = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned())
            .expect_err("the old table is an unknown key");
        assert!(!is_repo_forbidden(err.as_ref()), "{err}");
        let env = std::collections::HashMap::from([(
            "ZIRV_CTX_ARCHITECT_ENABLED".to_string(),
            "true".to_string(),
        )]);
        let clean = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::load(clean.path(), &|k| env.get(k).cloned()).expect("load");
        assert!(!cfg.supervisor.enabled, "the old env name is ignored");
    }

    #[test]
    fn enabling_without_a_model_is_a_config_error_at_load() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let env = std::collections::HashMap::from([(
            "ZIRV_CTX_SUPERVISOR_ENABLED".to_string(),
            "true".to_string(),
        )]);
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect_err("no model");
        assert!(err.to_string().contains("supervisor.model"), "{err}");
    }
}
