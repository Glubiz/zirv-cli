//! Private per-repository vault persistence and transaction locking.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup, ObfuscateMode};
use super::obfuscate::{Options, Vault, VaultEntry};
use super::state::StateDir;

struct LockGuard(PathBuf);

#[derive(Debug, clap::Args)]
pub struct ObfuscateArgs {
    #[command(subcommand)]
    pub command: ObfuscateCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ObfuscateCommand {
    /// List protected kinds, counts and first-seen surfaces; never values.
    List,
    /// Reveal one placeholder locally and record the audit action.
    Reveal { placeholder: String },
    /// Delete this repository's local placeholder vault.
    Purge,
    /// Scan a transcript file, a live session, or stdin without storing values.
    Scan {
        #[arg(value_name = "TRANSCRIPT", conflicts_with = "session")]
        transcript: Option<PathBuf>,
        #[arg(long, value_name = "SESSION", conflicts_with = "transcript")]
        session: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultSummary {
    pub values: usize,
    pub kinds: usize,
}

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

pub fn is_shared_placeholder_path(repo: &Path, path: &Path) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    };
    let rendered = path.to_string_lossy().replace('\\', "/");
    rendered.contains("/.zirv/work/")
        || rendered.ends_with("/.zirv/work")
        || rendered.contains("/.zirv/memory/")
        || rendered.ends_with("/.zirv/memory")
}

pub fn summary(state_root: &Path, repo: &Path) -> Option<VaultSummary> {
    let path = vault_path(state_root, repo);
    with_vault(&path, |vault| {
        let kinds = vault
            .entries()
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        Ok(VaultSummary {
            values: vault.entries().len(),
            kinds,
        })
    })
    .ok()
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
    let protected = obfuscate_text(state.root(), repo, text, &options, surface)?;
    if !protected.1.is_empty() {
        let mut counts = BTreeMap::<&str, usize>::new();
        for finding in &protected.1 {
            *counts.entry(&finding.kind).or_default() += 1;
        }
        let detail = format!(
            "{surface}: {}",
            counts
                .into_iter()
                .map(|(kind, count)| format!("{kind}:{count}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let session =
            std::env::var(super::adapters::SESSION_ENV).unwrap_or_else(|_| "operator".to_string());
        let _ = super::log::append(
            state,
            &super::log::Decision {
                ts: super::state::now_secs(),
                session: &session,
                verb: "obfuscate",
                verdict: "audit",
                score: 0,
                action: "obfuscate-surface",
                detail: &detail,
                observed_at: None,
            },
        );
    }
    Ok(protected)
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
            let original = std::mem::take(values);
            for (key, mut value) in original {
                let (key, mut key_findings) =
                    super::obfuscate::obfuscate(&key, vault, options, surface);
                findings.append(&mut key_findings);
                obfuscate_json_value(&mut value, vault, options, surface, findings);
                values.insert(key, value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

pub fn rehydrate_json(
    state_root: &Path,
    repo: &Path,
    value: &mut serde_json::Value,
) -> CtxResult<()> {
    with_vault(&vault_path(state_root, repo), |vault| {
        rehydrate_json_value(value, vault);
        if json_contains_placeholder(value) {
            return Err("unknown Zirv placeholder; refusing device action".into());
        }
        Ok(())
    })
}

fn json_contains_placeholder(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => super::obfuscate::contains_placeholder(text),
        serde_json::Value::Array(values) => values.iter().any(json_contains_placeholder),
        serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
            super::obfuscate::contains_placeholder(key) || json_contains_placeholder(value)
        }),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

pub fn run<W: Write>(args: &ObfuscateArgs, writer: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = super::config::env_from_process();
    let cfg = CtxConfig::load(&repo, &env)?;
    let state = StateDir::resolve(&env)?;
    let path = vault_path(state.root(), &repo);

    let (action, detail) = match &args.command {
        ObfuscateCommand::List => {
            let rows = with_vault(&path, |vault| {
                let mut rows: BTreeMap<(String, String, String), usize> = BTreeMap::new();
                for entry in vault.entries() {
                    *rows
                        .entry((
                            entry.kind.clone(),
                            format!("{:?}", entry.class),
                            entry.first_seen_surface.clone(),
                        ))
                        .or_default() += 1;
                }
                Ok(rows.into_iter().collect::<Vec<_>>())
            })?;
            for ((kind, class, first_seen), count) in &rows {
                writeln!(writer, "{kind}\t{class}\t{count}\t{first_seen}")?;
            }
            ("obfuscate-list", format!("{} kind row(s)", rows.len()))
        }
        ObfuscateCommand::Reveal { placeholder } => {
            let canonical = placeholder.split('@').next().unwrap_or(placeholder);
            let value = with_vault(&path, |vault| {
                vault
                    .entries()
                    .iter()
                    .find(|entry| entry.placeholder == canonical)
                    .map(|entry| entry.value.clone())
                    .ok_or_else(|| format!("unknown placeholder: {placeholder}").into())
            })?;
            writeln!(writer, "{value}")?;
            (
                "obfuscate-reveal",
                format!("revealed placeholder {canonical}"),
            )
        }
        ObfuscateCommand::Purge => {
            let _lock = acquire_lock(&path)?;
            match std::fs::remove_file(&path) {
                Ok(()) => writeln!(writer, "purged repository obfuscation vault")?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    writeln!(writer, "no repository obfuscation vault")?
                }
                Err(error) => return Err(error.into()),
            }
            (
                "obfuscate-purge",
                "repository placeholder vault purged".to_string(),
            )
        }
        ObfuscateCommand::Scan {
            transcript,
            session,
        } => {
            let (input, scanned_surface) =
                scan_input(transcript.as_deref(), session.as_deref(), &state, &cfg)?;
            let home = crate::utils::home_dir()?;
            let mut options = options_from_config(&cfg.obfuscate, &home)?;
            options.mode = super::obfuscate::Mode::Flag;
            let mut vault = Vault::default();
            let (_, findings) =
                super::obfuscate::obfuscate(&input, &mut vault, &options, &scanned_surface);
            let mut counts = BTreeMap::<String, usize>::new();
            for finding in findings {
                *counts.entry(finding.kind).or_default() += 1;
            }
            if counts.is_empty() {
                writeln!(writer, "clean")?;
            } else {
                for (kind, count) in &counts {
                    writeln!(writer, "{kind}\t{count}\t{scanned_surface}")?;
                }
            }
            (
                "obfuscate-scan",
                format!("{} sensitive kind(s)", counts.len()),
            )
        }
    };

    let session = env(super::adapters::SESSION_ENV).unwrap_or_else(|| "operator".to_string());
    let _ = super::log::append(
        &state,
        &super::log::Decision {
            ts: super::state::now_secs(),
            session: &session,
            verb: "obfuscate",
            verdict: "audit",
            score: 0,
            action,
            detail: &detail,
            observed_at: None,
        },
    );
    Ok(0)
}

fn scan_input(
    transcript: Option<&Path>,
    session: Option<&str>,
    state: &StateDir,
    cfg: &CtxConfig,
) -> CtxResult<(String, String)> {
    if let Some(path) = transcript {
        return Ok((
            std::fs::read_to_string(path)?,
            format!("transcript:{}", path.display()),
        ));
    }
    if let Some(prefix) = session {
        let record = super::sessions::resolve_prefix(state, prefix)
            .map_err(|error| format!("could not resolve session {prefix}: {error}"))?;
        if record.agent == super::runtime::RuntimeKind::Native.as_str() {
            return Err("native session scanning is not available from the transcript audit; scan an exported journal file instead".into());
        }
        let adapter = super::adapters::select(Some(&record.agent), &[], cfg)?;
        let path = adapter.transcript_path(&super::event::SessionRef {
            id: super::event::SessionId::parse(&record.session),
            cwd: record.repo,
        });
        return std::fs::read_to_string(&path)
            .map(|input| (input, format!("session:{prefix}")))
            .map_err(|error| {
                format!("could not read transcript {}: {error}", path.display()).into()
            });
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    Ok((input, "stdin".to_string()))
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
            let original = std::mem::take(values);
            for (key, mut value) in original {
                rehydrate_json_value(&mut value, vault);
                values.insert(super::obfuscate::rehydrate(&key, vault), value);
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
