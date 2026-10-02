//! Descriptor registry, per-key layer stack and scoped writers behind `/settings` (issue #536).
//!
//! Nothing here is a second config system: keys come from `ENV_MAP`, the trust bit from
//! `REPO_FORBIDDEN`, and every write goes through `config_cmd::apply_edit`, the same validated,
//! comment-preserving path `zirv ctx config` uses.

use super::super::config_cmd::{self, EditOp};
use super::repo_layer::EnvKind;
use super::*;

/// When a saved value takes effect. No key has a proven live reload yet, so every key says the
/// conservative thing instead of guessing.
pub const APPLIES: &str = "next session";

/// Curated descriptions; every other key falls back to its type and environment variable.
const DESCRIPTIONS: &[(&str, &str)] = &[
    ("agent", "Default agent harness for new sessions."),
    (
        "score.window",
        "Number of recent turns the rot score looks at.",
    ),
    (
        "pace.enabled",
        "Pause launches while a usage window is exhausted.",
    ),
    ("pace.max_wait_secs", "Longest launch pause."),
    (
        "fallback.enabled",
        "Automatic vendor steering between harnesses.",
    ),
    (
        "sandbox.scrub_worker_secrets",
        "Launch delegated workers without secret-shaped environment variables.",
    ),
    ("memory.enabled", "Durable memory bank."),
    ("mail.enabled", "Inter-agent mailbox."),
    ("dash.max_panes", "Most panes the dashboard opens."),
    ("dash.mouse", "Mouse support in the dashboard."),
    (
        "runtime.default",
        "Backend an unflagged session gets: harness or native.",
    ),
    (
        "worker.max_depth",
        "Deepest delegation chain a worker may start.",
    ),
    (
        "worker.deny_network",
        "Deny network access to delegated workers.",
    ),
    ("prompt.skill_index", "Standing skill index in the prompt."),
    ("prompt.intake_discipline", "First-prompt discipline note."),
    ("obfuscate.mode", "Secret masking: off, flag or obfuscate."),
    (
        "proxy.typesafe.credential_env",
        "Name of the variable holding the credential; the secret itself is never stored here.",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub fn parse(raw: &str) -> CtxResult<Self> {
        match raw {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            other => Err(format!("unknown scope '{other}': expected user or project").into()),
        }
    }

    pub fn letter(self) -> char {
        match self {
            Self::User => 'U',
            Self::Project => 'P',
        }
    }
}

#[derive(Clone, Debug)]
pub struct Setting {
    pub key: String,
    pub path: &'static [&'static str],
    pub env: &'static str,
    pub kind: &'static str,
    pub description: String,
    /// The `REPO_FORBIDDEN` hint when a repository may not set this key.
    pub operator_only: Option<&'static str>,
    pub sensitive: bool,
}

impl Setting {
    fn new(path: &'static [&'static str], env: &'static str, kind: &'static str) -> Self {
        let key = path.join(".");
        let description = DESCRIPTIONS
            .iter()
            .find(|(name, _)| *name == key)
            .map_or_else(
                || format!("{kind} setting (env {env})"),
                |(_, text)| (*text).to_string(),
            );
        Self {
            sensitive: kind != "bool" && is_secret_name(&key),
            operator_only: REPO_FORBIDDEN
                .iter()
                .find(|(forbidden, _)| path.starts_with(forbidden))
                .map(|(_, hint)| *hint),
            key,
            path,
            env,
            kind,
            description,
        }
    }

    /// Scopes a value may be written at; sensitive keys are never edited in ordinary fields.
    pub fn scopes(&self) -> Vec<Scope> {
        if self.sensitive {
            return Vec::new();
        }
        let mut scopes = vec![Scope::User];
        if self.operator_only.is_none() {
            scopes.push(Scope::Project);
        }
        scopes
    }
}

fn kind_label(kind: EnvKind) -> &'static str {
    match kind {
        EnvKind::Int => "integer",
        EnvKind::Float => "number",
        EnvKind::Bool | EnvKind::NegatedBool => "bool",
        EnvKind::Str => "string",
        EnvKind::StringMap => "map",
        EnvKind::StringList => "list",
    }
}

/// One descriptor per `ENV_MAP` key, plus any `REPO_FORBIDDEN` key the environment map lacks.
pub fn registry() -> Vec<Setting> {
    let mut out: Vec<Setting> = ENV_MAP
        .iter()
        .map(|(env, path, kind)| Setting::new(path, env, kind_label(*kind)))
        .collect();
    for (path, _) in REPO_FORBIDDEN {
        if !out.iter().any(|setting| setting.path == *path) {
            out.push(Setting::new(path, "", "toml"));
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

pub fn find(key: &str) -> Option<Setting> {
    registry().into_iter().find(|setting| setting.key == key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Env,
    Project,
    User,
    Default,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Project => "project",
            Self::User => "user",
            Self::Default => "default",
        }
    }
}

/// Raw per-layer tables, so shadowing is data: `false`, zero and missing stay distinct.
#[derive(Debug, Default)]
pub struct LayerTables {
    user: toml::Table,
    project: toml::Table,
    env: toml::Table,
}

/// The value each layer holds for one key, highest precedence first.
#[derive(Debug)]
pub struct Layers<'a> {
    pub env: Option<&'a toml::Value>,
    pub project: Option<&'a toml::Value>,
    pub user: Option<&'a toml::Value>,
}

impl<'a> Layers<'a> {
    pub fn stack(&self) -> [(Layer, Option<&'a toml::Value>); 3] {
        [
            (Layer::Env, self.env),
            (Layer::Project, self.project),
            (Layer::User, self.user),
        ]
    }

    pub fn winner(&self) -> Option<(Layer, &'a toml::Value)> {
        self.stack()
            .into_iter()
            .find_map(|(layer, value)| value.map(|value| (layer, value)))
    }
}

impl LayerTables {
    pub fn layers(&self, setting: &Setting) -> Layers<'_> {
        Layers {
            env: value_at(&self.env, setting.path),
            project: value_at(&self.project, setting.path),
            user: value_at(&self.user, setting.path),
        }
    }
}

fn read_table(path: &Path) -> CtxResult<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map_err(|e| format!("{}: {}", path.display(), summarize_parse_error(&e)).into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(e.into()),
    }
}

fn project_path(repo: &Path) -> PathBuf {
    repo.join(crate::utils::SCRIPT_DIR_NAME)
        .join(CTX_CONFIG_FILE)
}

pub fn resolve_layers(repo: &Path, env: EnvLookup<'_>) -> CtxResult<LayerTables> {
    let mut env_table = toml::Table::new();
    for (var, path, kind) in ENV_MAP {
        if let Some(raw) = env(var)
            && let Ok(value) = env_value(&raw, *kind)
        {
            insert_path(&mut env_table, path, value);
        }
    }
    let project = if crate::utils::repo_is_home(repo) {
        toml::Table::new()
    } else {
        read_table(&project_path(repo))?
    };
    Ok(LayerTables {
        user: read_table(&operator_path()?)?,
        project,
        env: env_table,
    })
}

pub struct SettingsCtx<'a> {
    pub repo: &'a Path,
    pub env: EnvLookup<'a>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    pub key: String,
    pub kind: &'static str,
    pub value: String,
    pub source: &'static str,
    pub scopes: Vec<Scope>,
    pub applies: &'static str,
    pub description: String,
    pub shadowed: String,
    pub locked: Option<String>,
    pub sensitive: bool,
}

fn show(setting: &Setting, value: &toml::Value) -> String {
    if setting.sensitive {
        return "(redacted)".to_string();
    }
    match redact_value(value) {
        toml::Value::String(text) => format!("{text:?}"),
        other => other.to_string(),
    }
}

fn row(setting: &Setting, tables: &LayerTables) -> SettingRow {
    let layers = tables.layers(setting);
    let winner = layers.winner();
    let shadowed: Vec<String> = layers
        .stack()
        .into_iter()
        .filter(|(layer, value)| value.is_some() && Some(*layer) != winner.map(|(w, _)| w))
        .map(|(layer, value)| format!("{}={}", layer.name(), show(setting, value.unwrap())))
        .collect();
    let locked = match (winner, setting.sensitive, setting.operator_only) {
        (_, true, _) => Some("credential: set it through the environment, never here".to_string()),
        (Some((Layer::Env, _)), _, _) => Some(format!(
            "{} is set and wins over any saved value",
            setting.env
        )),
        (_, _, Some(hint)) => Some(format!("project scope refused: operator only ({hint})")),
        _ => None,
    };
    SettingRow {
        key: setting.key.clone(),
        kind: setting.kind,
        value: winner.map_or_else(
            || "(default)".to_string(),
            |(_, value)| show(setting, value),
        ),
        source: winner.map_or(Layer::Default, |(layer, _)| layer).name(),
        scopes: setting.scopes(),
        applies: APPLIES,
        description: setting.description.clone(),
        shadowed: if shadowed.is_empty() {
            "none".to_string()
        } else {
            shadowed.join(", ")
        },
        locked,
        sensitive: setting.sensitive,
    }
}

pub fn rows(ctx: &SettingsCtx<'_>) -> CtxResult<Vec<SettingRow>> {
    let tables = resolve_layers(ctx.repo, ctx.env)?;
    Ok(registry()
        .iter()
        .map(|setting| row(setting, &tables))
        .collect())
}

pub enum Change<'a> {
    Set(&'a str),
    Reset,
}

/// Why a project write is refused: the effective config must never get looser.
fn project_effect_check(
    ctx: &SettingsCtx<'_>,
    setting: &Setting,
    updated: &str,
    op: EditOp,
) -> CtxResult<()> {
    let path = project_path(ctx.repo);
    let key = &setting.key;
    let table: toml::Table = toml::from_str(updated)?;
    if op == EditOp::Set {
        reject_untrusted_keys(&table, &path)?;
        reject_untrusted_workspace_execution(&table, &path)?;
        // No generic "narrower than" order exists, so replacing a repo value is never proven safe.
        if let Some(old) = value_at(&read_table(&path)?, setting.path)
            && Some(old) != value_at(&table, setting.path)
        {
            return Err(format!(
                "{key}: the project already sets a value and a replacement could loosen it; edit {} by hand if you mean it",
                path.display()
            )
            .into());
        }
    }
    let before = CtxConfig::load(ctx.repo, ctx.env)?;
    let scratch = std::env::temp_dir().join(format!("zirv-settings-{}", uuid::Uuid::new_v4()));
    let candidate = scratch
        .join(crate::utils::SCRIPT_DIR_NAME)
        .join(CTX_CONFIG_FILE);
    let after = std::fs::create_dir_all(candidate.parent().unwrap_or(&scratch))
        .and_then(|()| std::fs::write(&candidate, updated))
        .map_err(Into::into)
        .and_then(|()| CtxConfig::load(&scratch, ctx.env));
    let _ = std::fs::remove_dir_all(&scratch);
    let unchanged = before == after?;
    if op == EditOp::Unset && !unchanged {
        return Err(format!(
            "{key}: resetting it would remove a restriction; edit {} by hand if you mean it",
            path.display()
        )
        .into());
    }
    if op == EditOp::Set && unchanged {
        return Err(format!(
            "{key}: a project value can only narrow, and this would change nothing (a user, environment or built-in value already is at least as strict)"
        )
        .into());
    }
    Ok(())
}

/// True when `raw` embeds a credential: URL userinfo or a token-like query parameter.
fn embeds_credentials(raw: &str) -> bool {
    raw.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}'))
        .filter_map(|word| word.split_once("://"))
        .any(|(_, rest)| {
            let (authority, tail) = rest
                .split_once(['/', '?'])
                .map_or((rest, ""), |(a, _)| (a, &rest[a.len()..]));
            authority.contains('@')
                || tail.split_once('?').is_some_and(|(_, query)| {
                    query
                        .split('&')
                        .any(|pair| is_secret_name(pair.split('=').next().unwrap_or("")))
                })
        })
}

/// The one rule for "this name holds a secret", on whole segments split on `_`, `.` and `-`.
/// A `*_env` key holds the NAME of a variable, but it is still treated as secret-shaped: it renders
/// `(redacted)` and is refused, the conservative reading of "credential-like keys".
fn is_secret_name(name: &str) -> bool {
    const ANY_SEGMENT: [&str; 14] = [
        "secret",
        "secrets",
        "password",
        "passwd",
        "passphrase",
        "credential",
        "credentials",
        "cred",
        "creds",
        "apikey",
        "bearer",
        "cookie",
        "cookies",
        "authorization",
    ];
    const LAST_SEGMENT: [&str; 5] = ["token", "key", "sig", "signature", "auth"];
    let name = name.to_ascii_lowercase();
    let segments: Vec<&str> = name.split(['_', '.', '-']).collect();
    segments.iter().any(|segment| ANY_SEGMENT.contains(segment))
        || segments
            .last()
            .is_some_and(|last| LAST_SEGMENT.contains(last))
}

/// True when a TOML table value, at any depth, has a secret-named string field.
fn has_secret_field(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .any(|(name, item)| (is_secret_name(name) && item.is_str()) || has_secret_field(item)),
        toml::Value::Array(items) => items.iter().any(has_secret_field),
        _ => false,
    }
}

/// Mask URL userinfo and token-like query values inside a string.
fn redact_text(text: &str) -> String {
    if !text.contains("://") {
        return text.to_string();
    }
    let mut out = String::new();
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let Some((scheme, rest)) = word.split_once("://") else {
            out.push_str(word);
            continue;
        };
        let (authority, tail) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let authority = authority
            .rsplit_once('@')
            .map_or(authority.to_string(), |(_, host)| format!("***@{host}"));
        let tail = match tail.split_once('?') {
            Some((path, query)) => {
                let query: Vec<String> = query
                    .split('&')
                    .map(|pair| match pair.split_once('=') {
                        Some((name, _)) if is_secret_name(name) => format!("{name}=***"),
                        _ => pair.to_string(),
                    })
                    .collect();
                format!("{path}?{}", query.join("&"))
            }
            None => tail.to_string(),
        };
        out.push_str(&format!("{scheme}://{authority}{tail}"));
    }
    out
}

fn redact_value(value: &toml::Value) -> toml::Value {
    match value {
        toml::Value::String(text) => toml::Value::String(redact_text(text)),
        toml::Value::Array(items) => toml::Value::Array(items.iter().map(redact_value).collect()),
        toml::Value::Table(table) => toml::Value::Table(
            table
                .iter()
                .map(|(name, item)| {
                    let item = if is_secret_name(name) && item.is_str() {
                        toml::Value::String("(redacted)".to_string())
                    } else {
                        redact_value(item)
                    };
                    (name.clone(), item)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Write one change at one scope. `zirv ctx config` validation applies byte for byte: user and
/// session go through the operator-document check, project through the narrowing-only rules first.
pub fn change(
    ctx: &SettingsCtx<'_>,
    key: &str,
    change: Change<'_>,
    scope: Scope,
) -> CtxResult<String> {
    let setting = find(key);
    let sensitive = setting
        .as_ref()
        .map_or_else(|| is_secret_name(key), |setting| setting.sensitive);
    if sensitive {
        return Err(
            format!("{key} is a credential setting: it is never shown or edited here").into(),
        );
    }
    let (raw, op) = match change {
        Change::Set(raw)
            if embeds_credentials(raw)
                || toml::from_str::<toml::Table>(&format!("v = {raw}"))
                    .is_ok_and(|table| has_secret_field(&table["v"])) =>
        {
            return Err(format!(
                "{key}: the value embeds a credential (URL userinfo, a token-like query parameter or a secret-named field); reference an environment variable instead"
            )
            .into());
        }
        Change::Set(raw) => (raw, EditOp::Set),
        Change::Reset => ("", EditOp::Unset),
    };
    let verb = if op == EditOp::Unset { "reset" } else { "set" };
    match scope {
        Scope::User => {
            let changed = config_cmd::apply_operator_edit(key, raw, op)?;
            Ok(format!(
                "{key}: {} in {} ({APPLIES})",
                if changed { verb } else { "unchanged" },
                operator_path()?.display()
            ))
        }
        Scope::Project => {
            let setting = setting.ok_or_else(|| format!("unknown setting '{key}'"))?;
            if crate::utils::repo_is_home(ctx.repo) {
                return Err("the project is the home directory: use the user scope".into());
            }
            let path = project_path(ctx.repo);
            let check = |updated: &str| project_effect_check(ctx, &setting, updated, op);
            let changed = config_cmd::apply_edit(&path, key, raw, op, &check)?;
            Ok(format!(
                "{key}: {} in {} ({APPLIES})",
                if changed { verb } else { "unchanged" },
                path.display()
            ))
        }
    }
}

/// What a `/settings ...` text form asks the pane to do.
pub enum Slash {
    Open(String),
    Notice(String),
}

fn detail(ctx: &SettingsCtx<'_>, key: &str) -> CtxResult<String> {
    let setting = find(key).ok_or_else(|| format!("unknown setting '{key}'"))?;
    let tables = resolve_layers(ctx.repo, ctx.env)?;
    let row = row(&setting, &tables);
    let scopes: String = row.scopes.iter().map(|scope| scope.letter()).collect();
    Ok(format!(
        "{} = {} ({}; source: {}; scopes: {}; applies {})\nshadowed: {}",
        row.key,
        row.value,
        row.kind,
        row.source,
        if scopes.is_empty() { "-" } else { &scopes },
        row.applies,
        row.shadowed
    ))
}

/// Parse `/settings`, `/settings <query>`, `get <key>`, `set <key> <value> [--scope s]` and
/// `reset <key> [--scope s]`; a refusal comes back as the notice, never as a write.
pub fn slash(ctx: &SettingsCtx<'_>, args: &str) -> Slash {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let mut scope = Scope::User;
    if let Some(at) = words.iter().position(|word| *word == "--scope") {
        let parsed = words
            .get(at + 1)
            .ok_or_else(|| "--scope needs user or project".into())
            .and_then(|raw| Scope::parse(raw));
        match parsed {
            Ok(parsed) => scope = parsed,
            Err(error) => return Slash::Notice(error.to_string()),
        }
        words.drain(at..(at + 2).min(words.len()));
    }
    let result = match words.as_slice() {
        ["get", key] => detail(ctx, key),
        ["set", key, value @ ..] if !value.is_empty() => {
            change(ctx, key, Change::Set(&value.join(" ")), scope)
        }
        ["reset", key] => change(ctx, key, Change::Reset, scope),
        ["get" | "set" | "reset", ..] => Err(
            "usage: /settings get <key> | set <key> <value> [--scope s] | reset <key> [--scope s]"
                .into(),
        ),
        _ => return Slash::Open(words.join(" ")),
    };
    Slash::Notice(result.unwrap_or_else(|error| error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::super::super::testenv::HomeGuard;
    use super::*;

    struct Fixture {
        _home: tempfile::TempDir,
        repo: tempfile::TempDir,
        _guard: HomeGuard,
    }

    impl Fixture {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let guard = HomeGuard::set(home.path());
            Self {
                repo: tempfile::tempdir().unwrap(),
                _home: home,
                _guard: guard,
            }
        }

        fn user(&self, text: &str) {
            let path = operator_path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn project_file(&self) -> PathBuf {
            project_path(self.repo.path())
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn with_ctx<T>(fx: &Fixture, env: EnvLookup<'_>, f: impl FnOnce(&SettingsCtx<'_>) -> T) -> T {
        f(&SettingsCtx {
            repo: fx.repo.path(),
            env,
        })
    }

    fn row_of(fx: &Fixture, env: EnvLookup<'_>, key: &str) -> SettingRow {
        with_ctx(fx, env, |ctx| rows(ctx).unwrap())
            .into_iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("no row for {key}"))
    }

    #[test]
    fn every_env_map_key_has_a_descriptor_and_every_forbidden_key_is_operator_only() {
        let settings = registry();
        for (env, path, _) in ENV_MAP {
            let setting = settings
                .iter()
                .find(|setting| setting.path == *path)
                .unwrap_or_else(|| panic!("{env} has no descriptor"));
            assert_eq!(setting.env, *env);
            assert!(!setting.description.is_empty());
        }
        let mut keys: Vec<_> = settings
            .iter()
            .map(|setting| setting.key.as_str())
            .collect();
        keys.dedup();
        assert_eq!(keys.len(), settings.len(), "duplicate descriptor key");
        for (path, _) in REPO_FORBIDDEN {
            let setting = settings
                .iter()
                .find(|setting| setting.path == *path)
                .unwrap_or_else(|| panic!("{} has no descriptor", path.join(".")));
            assert!(setting.operator_only.is_some(), "{}", setting.key);
            assert!(
                !setting.scopes().contains(&Scope::Project),
                "{}",
                setting.key
            );
        }
        let credential = find("proxy.typesafe.credential_env").unwrap();
        assert!(credential.sensitive && credential.scopes().is_empty());
    }

    #[test]
    fn layer_stack_keeps_false_zero_and_missing_distinct() {
        let fx = Fixture::new();
        fx.user("[prompt]\nskill_index = false\n[score]\nwindow = 0\n");
        let env = |name: &str| (name == "ZIRV_CTX_WINDOW").then(|| "7".to_string());
        let skill_index = row_of(&fx, &no_env, "prompt.skill_index");
        assert_eq!(
            (skill_index.value.as_str(), skill_index.source),
            ("false", "user")
        );
        let window = row_of(&fx, &no_env, "score.window");
        assert_eq!((window.value.as_str(), window.source), ("0", "user"));
        let missing = row_of(&fx, &no_env, "score.min_turns");
        assert_eq!(
            (missing.value.as_str(), missing.source),
            ("(default)", "default")
        );
        let shadowed = row_of(&fx, &env, "score.window");
        assert_eq!((shadowed.value.as_str(), shadowed.source), ("7", "env"));
        assert_eq!(shadowed.shadowed, "user=0");
        assert!(shadowed.locked.unwrap().contains("ZIRV_CTX_WINDOW"));
    }

    #[test]
    fn user_scope_round_trips_through_ctx_toml_and_keeps_comments() {
        let fx = Fixture::new();
        fx.user("# mine\n[score]\nwindow = 3 # keep\n");
        let message = with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "score.window", Change::Set("5"), Scope::User).unwrap()
        });
        assert!(message.contains(APPLIES), "{message}");
        let text = std::fs::read_to_string(operator_path().unwrap()).unwrap();
        assert_eq!(text, "# mine\n[score]\nwindow = 5 # keep\n");
        with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "score.window", Change::Reset, Scope::User).unwrap()
        });
        let text = std::fs::read_to_string(operator_path().unwrap()).unwrap();
        assert!(
            text.contains("# mine") && !text.contains("window"),
            "{text}"
        );
        assert_eq!(row_of(&fx, &no_env, "score.window").source, "default");
    }

    #[test]
    fn config_cli_and_settings_report_the_same_validation_error() {
        let fx = Fixture::new();
        let cli = config_cmd::run(
            &config_cmd::ConfigArgs {
                command: config_cmd::ConfigCommand::Set {
                    key: "memory.enabled".into(),
                    value: "nope".into(),
                },
            },
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        let error = with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "memory.enabled", Change::Set("nope"), Scope::User).unwrap_err()
        });
        assert_eq!(error.to_string(), cli);
        assert!(!operator_path().unwrap().exists());
    }

    #[test]
    fn project_scope_refuses_forbidden_and_widening_values_before_any_write() {
        let fx = Fixture::new();
        let forbidden = with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "memory.enabled", Change::Set("false"), Scope::Project).unwrap_err()
        });
        assert!(
            forbidden
                .to_string()
                .contains("may not be set by a repository"),
            "{forbidden}"
        );
        // The user layer already turned the index off; a project value cannot turn it back on.
        fx.user("[prompt]\nskill_index = false\n");
        let widening = with_ctx(&fx, &no_env, |ctx| {
            change(
                ctx,
                "prompt.skill_index",
                Change::Set("true"),
                Scope::Project,
            )
            .unwrap_err()
        });
        assert!(widening.to_string().contains("only narrow"), "{widening}");
        let unknown = with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "prompt.nope", Change::Set("true"), Scope::Project).unwrap_err()
        });
        assert!(unknown.to_string().contains("unknown setting"), "{unknown}");
        assert!(!fx.project_file().exists());
    }

    #[test]
    fn project_scope_writes_a_narrowing_value_and_shows_it_as_the_winner() {
        let fx = Fixture::new();
        with_ctx(&fx, &no_env, |ctx| {
            change(
                ctx,
                "prompt.skill_index",
                Change::Set("false"),
                Scope::Project,
            )
            .unwrap()
        });
        assert_eq!(
            std::fs::read_to_string(fx.project_file()).unwrap(),
            "[prompt]\nskill_index = false\n"
        );
        let row = row_of(&fx, &no_env, "prompt.skill_index");
        assert_eq!((row.value.as_str(), row.source), ("false", "project"));
    }

    #[test]
    fn project_set_never_replaces_an_existing_narrowing_with_a_wider_value() {
        let fx = Fixture::new();
        let path = fx.project_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[prompt]\nskill_index = false\n").unwrap();
        let error = with_ctx(&fx, &no_env, |ctx| {
            change(
                ctx,
                "prompt.skill_index",
                Change::Set("true"),
                Scope::Project,
            )
            .unwrap_err()
        });
        assert!(error.to_string().contains("by hand"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[prompt]\nskill_index = false\n"
        );
    }

    #[test]
    fn project_reset_is_refused_when_it_removes_a_narrowing_and_allowed_otherwise() {
        let fx = Fixture::new();
        let path = fx.project_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[prompt]\nskill_index = false\n").unwrap();
        let error = with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "prompt.skill_index", Change::Reset, Scope::Project).unwrap_err()
        });
        assert!(error.to_string().contains("by hand"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[prompt]\nskill_index = false\n"
        );
        // A key the project never set, and a project value the user layer already matches, change nothing.
        let absent = with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "score.window", Change::Reset, Scope::Project).unwrap()
        });
        assert!(absent.contains("unchanged"), "{absent}");
        fx.user("[prompt]\nskill_index = false\n");
        with_ctx(&fx, &no_env, |ctx| {
            change(ctx, "prompt.skill_index", Change::Reset, Scope::Project).unwrap()
        });
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("skill_index")
        );
    }

    #[test]
    fn endpoint_tables_render_redacted_and_embedded_credentials_are_refused_everywhere() {
        let fx = Fixture::new();
        fx.user(
            "[endpoint.claude]\nbase_url = \"https://bob:hunter2@api.example/v1?token=abc123&x=1\"\nauthorization = \"Bearer zzz\"\n",
        );
        let row = row_of(&fx, &no_env, "endpoint");
        for leaked in ["bob", "hunter2", "abc123", "zzz"] {
            assert!(
                !row.value.contains(leaked),
                "{leaked} leaked: {}",
                row.value
            );
        }
        assert!(
            row.value.contains("api.example") && row.value.contains("x=1"),
            "{}",
            row.value
        );
        for raw in [
            "https://bob:hunter2@api.example/v1",
            "https://api.example/v1?api_key=abc",
            "\"https://api.example/?x=1&password=p\"",
        ] {
            for scope in [Scope::User, Scope::Project] {
                let error = with_ctx(&fx, &no_env, |ctx| {
                    change(ctx, "proxy.typesafe.base_url", Change::Set(raw), scope).unwrap_err()
                });
                assert!(
                    error.to_string().contains("embeds a credential"),
                    "{raw}: {error}"
                );
            }
        }
        assert!(!embeds_credentials("https://api.example/v1?page=2"));
        assert!(!fx.project_file().exists());
    }

    #[test]
    fn credential_named_fields_are_redacted_refused_and_innocuous_names_are_not() {
        let fx = Fixture::new();
        fx.user("[endpoint.x]\nsearch_credential = \"abc\"\nname = \"ok\"\n");
        let row = row_of(&fx, &no_env, "endpoint");
        assert!(
            !row.value.contains("abc") && row.value.contains("ok"),
            "{}",
            row.value
        );
        for key in ["search_credential", "endpoint.search_credential"] {
            for scope in [Scope::User, Scope::Project] {
                let error = with_ctx(&fx, &no_env, |ctx| {
                    change(ctx, key, Change::Set("\"abc\""), scope).unwrap_err()
                });
                assert!(error.to_string().contains("credential"), "{key}: {error}");
            }
        }
        let table = with_ctx(&fx, &no_env, |ctx| {
            change(
                ctx,
                "endpoint",
                Change::Set("{ search_credential = \"abc\" }"),
                Scope::User,
            )
            .unwrap_err()
        });
        assert!(table.to_string().contains("embeds a credential"), "{table}");
        for name in [
            "credential",
            "api_key",
            "access-key",
            "session_token",
            "x.Cookie",
            "creds",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["credit_limit", "score.token_floor", "max_key_bytes"] {
            assert!(!is_secret_name(name), "{name}");
        }
        assert!(!find("sandbox.scrub_worker_secrets").unwrap().sensitive);
    }

    #[test]
    fn credentials_are_redacted_and_refused_in_every_scope() {
        let fx = Fixture::new();
        fx.user("[proxy.typesafe]\ncredential_env = \"MY_SECRET_ENV\"\n");
        let row = row_of(&fx, &no_env, "proxy.typesafe.credential_env");
        assert_eq!(row.value, "(redacted)");
        assert!(!row.shadowed.contains("MY_SECRET_ENV"));
        for scope in [Scope::User, Scope::Project] {
            let error = with_ctx(&fx, &no_env, |ctx| {
                change(
                    ctx,
                    "proxy.typesafe.credential_env",
                    Change::Set("x"),
                    scope,
                )
                .unwrap_err()
            });
            assert!(error.to_string().contains("credential"), "{error}");
        }
        let notice = with_ctx(&fx, &no_env, |ctx| {
            match slash(ctx, "get proxy.typesafe.credential_env") {
                Slash::Notice(text) => text,
                Slash::Open(_) => panic!("get must not open the menu"),
            }
        });
        assert!(!notice.contains("MY_SECRET_ENV"), "{notice}");
    }

    #[test]
    fn slash_forms_parse_get_set_reset_scope_and_queries() {
        let fx = Fixture::new();
        let run = |args: &str| {
            with_ctx(&fx, &no_env, |ctx| match slash(ctx, args) {
                Slash::Notice(text) => format!("notice:{text}"),
                Slash::Open(query) => format!("open:{query}"),
            })
        };
        assert_eq!(run(""), "open:");
        assert_eq!(run("pace wait"), "open:pace wait");
        assert!(run("set score.window 4").contains("set"));
        assert!(run("get score.window").contains("score.window = 4 (integer; source: user"));
        assert!(run("set score.window 6 --scope session").contains("unknown scope"));
        assert!(run("reset score.window").contains("reset"));
        assert!(run("set score.window 1 --scope galaxy").contains("unknown scope"));
        assert!(run("set score.window").contains("usage"));
        assert!(run("get nope.nope").contains("unknown setting"));
    }
}
