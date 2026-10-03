//! Secret-shaped environment scrubbing for delegated workers.

use std::collections::BTreeSet;
use std::process::Command;

/// Name fragments that mark a variable as a credential, matched case-insensitively.
const SECRET_FRAGMENTS: [&str; 9] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "CREDENTIAL",
];

/// `AUTH` only counts as a whole `_`-separated word, so `GIT_AUTHOR_NAME` and `XAUTHORITY` survive.
const SECRET_WORDS: [&str; 3] = ["AUTH", "AUTHORIZATION", "OAUTH"];

const SECRET_EXACT: [&str; 11] = [
    "SSH_AUTH_SOCK",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_CLIENT_SECRET",
    "DOCKER_AUTH_CONFIG",
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "VAULT_TOKEN",
    "CARGO_REGISTRY_TOKEN",
];

/// Process context a worker always needs, whatever its name looks like.
const PROTECTED: [&str; 5] = ["PATH", "HOME", "USERPROFILE", "TERM", "LANG"];

pub(crate) fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("ZIRV_") || upper.starts_with("LC_") || PROTECTED.contains(&upper.as_str())
    {
        return false;
    }
    SECRET_EXACT.contains(&upper.as_str())
        || SECRET_FRAGMENTS.iter().any(|f| upper.contains(f))
        || upper.split('_').any(|word| SECRET_WORDS.contains(&word))
}

/// Names to strip: secret-shaped ambient names that are neither the harness's own credentials
/// (`keep`, `None` = the adapter declares none, so nothing is touched) nor set explicitly.
fn names_to_strip(
    enabled: bool,
    keep: Option<&[String]>,
    explicit: BTreeSet<String>,
    ambient: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let Some(keep) = keep.filter(|_| enabled) else {
        return Vec::new();
    };
    let keep: BTreeSet<String> = keep.iter().map(|k| k.to_ascii_uppercase()).collect();
    let mut stripped: Vec<String> = ambient
        .into_iter()
        .filter(|name| is_secret_env_name(name))
        .filter(|name| {
            let upper = name.to_ascii_uppercase();
            !keep.contains(&upper) && !explicit.contains(&upper)
        })
        .collect();
    stripped.sort();
    stripped.dedup();
    stripped
}

/// Remove secret-shaped names from a worker's `Command`. Returns the stripped NAMES, never values.
pub(crate) fn scrub_worker_secrets_cmd(
    command: &mut Command,
    enabled: bool,
    keep: Option<&[String]>,
    ambient: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let explicit = command
        .get_envs()
        .filter(|(_, value)| value.is_some())
        .map(|(key, _)| key.to_string_lossy().to_ascii_uppercase())
        .collect();
    let stripped = names_to_strip(enabled, keep, explicit, ambient);
    for name in &stripped {
        command.env_remove(name);
    }
    stripped
}

/// The `portable_pty` twin of [`scrub_worker_secrets_cmd`]; call it before the launch's own env is
/// applied, since a `CommandBuilder` cannot tell inherited from explicit values.
pub(crate) fn scrub_worker_secrets_pty(
    builder: &mut portable_pty::CommandBuilder,
    enabled: bool,
    keep: Option<&[String]>,
    ambient: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let stripped = names_to_strip(enabled, keep, BTreeSet::new(), ambient);
    for name in &stripped {
        builder.env_remove(name);
    }
    stripped
}

/// The launch-seam entry: the operator's `[sandbox] scrub_worker_secrets`, the adapter's own
/// credentials and the real process env, in one call. Returns the stripped NAMES. `keep_jev_key`
/// is for a launch with no relay to the supervisor, so a hook-side `[jev]` gate must hold the key itself.
pub(crate) fn scrub_worker_env_cmd(
    command: &mut Command,
    cfg: &super::super::config::CtxConfig,
    adapter: &dyn super::super::adapters::AgentAdapter,
    keep_jev_key: bool,
) -> Vec<String> {
    let mut keep = adapter.credential_env(&super::super::config::env_from_process());
    if let Some(keep) = keep.as_mut().filter(|_| keep_jev_key) {
        keep.push(cfg.proxy.typesafe.credential_env.clone());
    }
    let ambient = std::env::vars_os().filter_map(|(name, _)| name.into_string().ok());
    scrub_worker_secrets_cmd(
        command,
        cfg.sandbox.scrub_worker_secrets,
        keep.as_deref(),
        ambient,
    )
}

/// Record the stripped NAMES in the decision log; nothing is logged when nothing was stripped.
pub(crate) fn log_withheld(
    state: &super::super::state::StateDir,
    session: &str,
    verb: &str,
    stripped: &[String],
) {
    if stripped.is_empty() {
        return;
    }
    let _ = super::super::log::append(
        state,
        &super::super::log::Decision {
            ts: super::super::state::now_secs(),
            session,
            verb,
            verdict: "n/a",
            score: 0,
            action: "scrub-env",
            detail: &format!("withheld from worker: {}", stripped.join(", ")),
            observed_at: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn parent_env() -> Vec<String> {
        [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "MY_SERVICE_PASSWORD",
            "ANTHROPIC_API_KEY",
            "PATH",
            "HOME",
            "ZIRV_CTX_SESSION",
            "ZIRV_TEST_TOKEN",
            "GIT_AUTHOR_NAME",
        ]
        .map(String::from)
        .to_vec()
    }

    fn removed(command: &Command) -> BTreeSet<String> {
        command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn worker_launch_strips_secrets_and_keeps_harness_credential_and_context() {
        let mut command = Command::new("agent");
        command.env("ZIRV_CTX_SESSION", "s1");
        let keep = vec!["ANTHROPIC_API_KEY".to_string()];
        let stripped = scrub_worker_secrets_cmd(&mut command, true, Some(&keep), parent_env());
        let want: BTreeSet<String> = [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "MY_SERVICE_PASSWORD",
        ]
        .map(String::from)
        .into();
        assert_eq!(removed(&command), want);
        assert_eq!(stripped.into_iter().collect::<BTreeSet<_>>(), want);
        assert_eq!(
            command
                .get_envs()
                .find(|(k, _)| *k == OsStr::new("ZIRV_CTX_SESSION"))
                .and_then(|(_, v)| v),
            Some(OsStr::new("s1"))
        );
    }

    #[test]
    fn an_explicitly_set_launch_variable_is_never_removed() {
        let mut command = Command::new("agent");
        command.env("ANTHROPIC_AUTH_TOKEN", "endpoint-key");
        let env = vec!["ANTHROPIC_AUTH_TOKEN".to_string()];
        assert!(scrub_worker_secrets_cmd(&mut command, true, Some(&[]), env).is_empty());
        assert!(removed(&command).is_empty());
    }

    #[test]
    fn opting_out_or_an_undeclared_harness_keeps_everything() {
        let mut disabled = Command::new("agent");
        let keep = vec!["ANTHROPIC_API_KEY".to_string()];
        assert!(
            scrub_worker_secrets_cmd(&mut disabled, false, Some(&keep), parent_env()).is_empty()
        );
        assert!(removed(&disabled).is_empty());
        let mut undeclared = Command::new("agent");
        assert!(scrub_worker_secrets_cmd(&mut undeclared, true, None, parent_env()).is_empty());
        assert!(removed(&undeclared).is_empty());
    }

    #[test]
    fn pty_builder_loses_secrets_but_keeps_the_harness_credential() {
        let mut builder = portable_pty::CommandBuilder::new("agent");
        builder.env("MY_SERVICE_PASSWORD", "x");
        builder.env("ANTHROPIC_API_KEY", "k");
        let keep = vec!["ANTHROPIC_API_KEY".to_string()];
        let env = vec![
            "MY_SERVICE_PASSWORD".to_string(),
            "ANTHROPIC_API_KEY".to_string(),
        ];
        let stripped = scrub_worker_secrets_pty(&mut builder, true, Some(&keep), env);
        assert_eq!(stripped, vec!["MY_SERVICE_PASSWORD".to_string()]);
        assert!(builder.get_env("MY_SERVICE_PASSWORD").is_none());
        assert!(builder.get_env("ANTHROPIC_API_KEY").is_some());
    }

    #[test]
    fn secret_shape_matches_case_insensitively_and_spares_context() {
        for secret in [
            "aws_secret_access_key",
            "Npm_Token",
            "DB_PASSWD",
            "STRIPE_APIKEY",
            "DOCKER_AUTH_CONFIG",
            "SOME_PRIVATE_KEY_FILE",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(is_secret_env_name(secret), "{secret}");
        }
        for plain in [
            "PATH",
            "path",
            "HOME",
            "USERPROFILE",
            "TERM",
            "LANG",
            "LC_ALL",
            "ZIRV_CTX_SOCKET",
            "ZIRV_TEST_TOKEN",
            "GIT_AUTHOR_NAME",
            "XAUTHORITY",
            "EDITOR",
        ] {
            assert!(!is_secret_env_name(plain), "{plain}");
        }
    }

    #[test]
    fn the_jev_key_is_kept_only_when_the_launch_asks_and_other_keys_stay_withheld() {
        let mut cfg = crate::commands::ctx::config::CtxConfig::default();
        cfg.proxy.typesafe.credential_env = "JEV870_API_KEY".to_string();
        let adapter = crate::commands::ctx::adapters::select(Some("claude"), &[], &cfg)
            .expect("claude adapter");
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::set_var("JEV870_API_KEY", "x");
            std::env::set_var("OTHER870_API_KEY", "x");
        }
        let mut without = Command::new("agent");
        scrub_worker_env_cmd(&mut without, &cfg, adapter.as_ref(), false);
        let mut with = Command::new("agent");
        scrub_worker_env_cmd(&mut with, &cfg, adapter.as_ref(), true);
        unsafe {
            std::env::remove_var("JEV870_API_KEY");
            std::env::remove_var("OTHER870_API_KEY");
        }
        assert!(removed(&without).contains("JEV870_API_KEY"));
        assert!(!removed(&with).contains("JEV870_API_KEY"));
        assert!(removed(&with).contains("OTHER870_API_KEY"));
    }
}
