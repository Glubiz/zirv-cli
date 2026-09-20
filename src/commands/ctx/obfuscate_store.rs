//! Private per-repository vault persistence and transaction locking.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup, ObfuscateMode};
use super::obfuscate::{Options, Vault, VaultEntry};
use super::state::StateDir;

struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn vault_path(state_root: &Path, repo: &Path) -> PathBuf {
    state_root
        .join("obfuscate")
        .join(format!("{}.jsonl", super::state::repo_slug(repo)))
}

pub fn options_from_config(
    config: &super::config::ObfuscateConfig,
    home: &Path,
) -> CtxResult<Options> {
    let literals = match config.literals_file.as_deref() {
        None => Vec::new(),
        Some(path) => {
            let path = if let Some(rest) = path.strip_prefix("~/") {
                home.join(rest)
            } else {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    home.join(path)
                }
            };
            std::fs::read_to_string(&path)?
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string)
                .collect()
        }
    };
    Ok(config.options(literals))
}

/// The shared protection boundary for text Zirv is about to compose into a
/// model prompt, persist in a shared surface, or send over the network.
/// `off` is a byte-identical pass-through; every other mode uses the same
/// per-repository vault and detector configuration.
pub fn protect_text(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    text: &str,
    surface: &str,
) -> CtxResult<(String, Vec<super::obfuscate::Finding>)> {
    if cfg.obfuscate.mode == ObfuscateMode::Off {
        return Ok((text.to_string(), Vec::new()));
    }
    let home = crate::utils::home_dir()?;
    let options = options_from_config(&cfg.obfuscate, &home)?;
    obfuscate_text(state.root(), repo, text, &options, surface)
}

pub fn protect_text_with_env(
    repo: &Path,
    text: &str,
    surface: &str,
    env: EnvLookup<'_>,
) -> CtxResult<(String, Vec<super::obfuscate::Finding>)> {
    let cfg = CtxConfig::load(repo, env)?;
    let state = StateDir::resolve(env)?;
    protect_text(&state, repo, &cfg, text, surface)
}

pub fn protect_composed(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    mut composed: Option<super::prompt::ComposedPrompt>,
    surface: &str,
) -> CtxResult<Option<super::prompt::ComposedPrompt>> {
    if let Some(prompt) = composed.as_mut() {
        prompt.text = protect_text(state, repo, cfg, &prompt.text, surface)?.0;
    }
    Ok(composed)
}

/// Holds the cross-process lock for the complete load-transform-save
/// transaction. No caller can observe and then overwrite another process's
/// newly assigned placeholder.
pub fn with_vault<T>(
    path: &Path,
    transform: impl FnOnce(&mut Vault) -> CtxResult<T>,
) -> CtxResult<T> {
    if let Some(parent) = path.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    let _lock = acquire_lock(path)?;
    let mut vault = load(path)?;
    let result = transform(&mut vault)?;
    save(path, &vault)?;
    Ok(result)
}

pub fn obfuscate_text(
    state_root: &Path,
    repo: &Path,
    text: &str,
    options: &Options,
    surface: &str,
) -> CtxResult<(String, Vec<super::obfuscate::Finding>)> {
    with_vault(&vault_path(state_root, repo), |vault| {
        Ok(super::obfuscate::obfuscate(text, vault, options, surface))
    })
}

pub fn obfuscate_json(
    state_root: &Path,
    repo: &Path,
    value: &mut serde_json::Value,
    options: &Options,
    surface: &str,
) -> CtxResult<Vec<super::obfuscate::Finding>> {
    with_vault(&vault_path(state_root, repo), |vault| {
        let mut findings = Vec::new();
        obfuscate_json_value(value, vault, options, surface, &mut findings);
        Ok(findings)
    })
}

fn obfuscate_json_value(
    value: &mut serde_json::Value,
    vault: &mut Vault,
    options: &Options,
    surface: &str,
    findings: &mut Vec<super::obfuscate::Finding>,
) {
    match value {
        serde_json::Value::String(text) => {
            let (masked, mut found) = super::obfuscate::obfuscate(text, vault, options, surface);
            *text = masked;
            findings.append(&mut found);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                obfuscate_json_value(value, vault, options, surface, findings);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                obfuscate_json_value(value, vault, options, surface, findings);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

pub fn rehydrate_text(state_root: &Path, repo: &Path, text: &str) -> CtxResult<String> {
    with_vault(&vault_path(state_root, repo), |vault| {
        Ok(super::obfuscate::rehydrate(text, vault))
    })
}

pub fn rehydrate_json(
    state_root: &Path,
    repo: &Path,
    value: &mut serde_json::Value,
) -> CtxResult<()> {
    with_vault(&vault_path(state_root, repo), |vault| {
        rehydrate_json_value(value, vault);
        Ok(())
    })
}

fn rehydrate_json_value(value: &mut serde_json::Value, vault: &Vault) {
    match value {
        serde_json::Value::String(text) => *text = super::obfuscate::rehydrate(text, vault),
        serde_json::Value::Array(values) => {
            for value in values {
                rehydrate_json_value(value, vault);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                rehydrate_json_value(value, vault);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn load(path: &Path) -> CtxResult<Vault> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vault::default()),
        Err(error) => return Err(error.into()),
    };
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: VaultEntry = serde_json::from_str(line)
            .map_err(|error| format!("invalid obfuscation vault row {}: {error}", index + 1))?;
        entries.push(entry);
    }
    Vault::from_entries(entries).map_err(Into::into)
}

fn save(path: &Path, vault: &Vault) -> CtxResult<()> {
    let mut text = String::new();
    for entry in vault.entries() {
        text.push_str(&serde_json::to_string(entry)?);
        text.push('\n');
    }
    super::state::write_private(path, &text)?;
    Ok(())
}

fn acquire_lock(vault_path: &Path) -> CtxResult<LockGuard> {
    let lock_path = vault_path.with_extension("jsonl.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&lock_path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(LockGuard(lock_path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if stale(&lock_path) {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out locking obfuscation vault {}",
                        vault_path.display()
                    )
                    .into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(120))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_rejects_corrupt_rows_and_persists_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vault.jsonl");
        std::fs::write(&path, "{bad json}\n").expect("fixture");
        assert!(with_vault(&path, |_| Ok(())).is_err());

        std::fs::remove_file(&path).expect("remove fixture");
        let options = Options::default();
        with_vault(&path, |vault| {
            let _ = super::super::obfuscate::obfuscate("jane@company.dk", vault, &options, "test");
            Ok(())
        })
        .expect("persist");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
