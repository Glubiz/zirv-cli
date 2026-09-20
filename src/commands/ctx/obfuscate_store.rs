//! Private per-repository vault persistence and transaction locking.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::CtxResult;
use super::obfuscate::{Options, Vault, VaultEntry};

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

pub fn rehydrate_text(state_root: &Path, repo: &Path, text: &str) -> CtxResult<String> {
    with_vault(&vault_path(state_root, repo), |vault| {
        Ok(super::obfuscate::rehydrate(text, vault))
    })
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
        let entry: VaultEntry = serde_json::from_str(line).map_err(|error| {
            format!("invalid obfuscation vault row {}: {error}", index + 1)
        })?;
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
                    return Err(format!("timed out locking obfuscation vault {}", vault_path.display()).into());
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
            let _ = super::super::obfuscate::obfuscate(
                "jane@company.dk", vault, &options, "test",
            );
            Ok(())
        }).expect("persist");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o600);
        }
    }
}
