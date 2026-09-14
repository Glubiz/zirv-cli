//! Comment-preserving edits to the operator's ctx config.

use std::io::Write;

use toml_edit::{DocumentMut, Item, Value};

use super::{CtxResult, config, state};

#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ConfigCommand {
    /// Print operator ~/.zirv/ctx.toml, or one dotted key, as TOML.
    Show { key: Option<String> },
    /// Set a dotted key; parse VALUE as TOML, otherwise as a string. Asks for approval.
    Set {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Append one array element, creating the array if absent; duplicates are unchanged. Asks for approval.
    Add {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Bring ~/.zirv/ctx.toml to the current schema, backing up the document
    /// it replaced; `--downgrade` restores that backup. Idempotent in both
    /// directions: a second run writes nothing (issue #491).
    Migrate {
        /// Which backend an unflagged session gets afterwards. The default
        /// preserves today's behaviour exactly; `native` is the opt-in.
        #[arg(long, value_name = "RUNTIME", default_value = "harness")]
        to: String,
        /// Restore the pre-migration document and drop the schema marker.
        #[arg(long)]
        downgrade: bool,
        /// Report what would change and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// The schema `zirv ctx config migrate` brings `~/.zirv/ctx.toml` to.
///
/// 1 is every operator config written before issue #491 -- no `[runtime]`
/// table, so `runtime::resolve` answers `harness` for everything. 2 adds the
/// `[runtime]` table that lets an unflagged session run natively.
///
/// The marker lives in a SIDECAR file, never in `ctx.toml` itself, and that
/// is deliberate: `CtxConfig` is `deny_unknown_fields`, so a `schema` key
/// inside `ctx.toml` would make an older zirv binary reject the operator's
/// whole configuration instead of merely ignoring a key it has not heard of.
/// The downgrade path exists for the same reason -- an older binary still
/// cannot read a `[runtime]` table, so stepping back restores the document
/// that predates it rather than editing around it.
pub const CTX_SCHEMA: u32 = 2;

fn migration_path(ctx_toml: &std::path::Path) -> std::path::PathBuf {
    ctx_toml.with_file_name("ctx.migration.toml")
}

fn backup_path(ctx_toml: &std::path::Path) -> std::path::PathBuf {
    ctx_toml.with_file_name(format!("ctx.toml.pre-schema-{CTX_SCHEMA}.bak"))
}

/// The schema recorded for this machine. An absent or unreadable sidecar is
/// schema 1 -- the state every machine was in before N22.
fn recorded_schema(ctx_toml: &std::path::Path) -> u32 {
    std::fs::read_to_string(migration_path(ctx_toml))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| doc.get("schema").and_then(Item::as_integer))
        .and_then(|schema| u32::try_from(schema).ok())
        .unwrap_or(1)
}

/// Pure: the schema-2 document for `text`, or `None` when it already is one.
/// Comment-preserving, like every other edit in this module.
pub fn migrate_document(text: &str, to: &str) -> CtxResult<Option<String>> {
    if !matches!(to, "harness" | "native") {
        return Err(format!("--to '{to}': expected `harness` or `native`").into());
    }
    let mut doc: DocumentMut = text.parse()?;
    if doc
        .get("runtime")
        .and_then(|runtime| runtime.get("default"))
        .is_some()
    {
        return Ok(None);
    }
    let target = slot(doc.as_item_mut(), &key_parts("runtime.default")?)?;
    *target = Item::Value(Value::from(to));
    Ok(Some(doc.to_string()))
}

/// Pure: `text` with the whole `[runtime]` table removed, or `None` when it
/// has none. The no-backup downgrade path; the backup path restores bytes.
pub fn strip_runtime_table(text: &str) -> CtxResult<Option<String>> {
    let mut doc: DocumentMut = text.parse()?;
    if doc.get("runtime").is_none() {
        return Ok(None);
    }
    doc.remove("runtime");
    Ok(Some(doc.to_string()))
}

fn migrate(
    path: &std::path::Path,
    to: &str,
    downgrade: bool,
    dry_run: bool,
    w: &mut dyn Write,
) -> CtxResult<i32> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let backup = backup_path(path);
    let marker = migration_path(path);
    if downgrade {
        if recorded_schema(path) <= 1 && strip_runtime_table(&text)?.is_none() {
            writeln!(w, "already at schema 1 (unchanged)")?;
            return Ok(0);
        }
        let restored = match std::fs::read_to_string(&backup) {
            Ok(restored) => restored,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => strip_runtime_table(&text)?
                .unwrap_or_else(|| text.clone()),
            Err(e) => return Err(e.into()),
        };
        config::validate_operator_document(&restored)
            .map_err(|e| format!("refusing to downgrade {}: {e}", path.display()))?;
        if dry_run {
            writeln!(w, "would restore schema 1 into {}", path.display())?;
            return Ok(0);
        }
        state::write_private(path, &restored)?;
        // Both markers go, so a later `migrate` starts clean rather than
        // restoring a backup that no longer matches anything.
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_file(&marker);
        writeln!(
            w,
            "restored schema 1 into {} (native journals, native.toml and session state untouched)",
            path.display()
        )?;
        return Ok(0);
    }
    let Some(updated) = migrate_document(&text, to)? else {
        writeln!(w, "already at schema {CTX_SCHEMA} (unchanged)")?;
        return Ok(0);
    };
    if recorded_schema(path) >= CTX_SCHEMA {
        // The marker says migrated but the table is gone: the operator
        // removed it by hand, which is a legitimate way to switch back.
        writeln!(w, "already at schema {CTX_SCHEMA} (unchanged)")?;
        return Ok(0);
    }
    config::validate_operator_document(&updated)
        .map_err(|e| format!("refusing to migrate {}: {e}", path.display()))?;
    if dry_run {
        writeln!(
            w,
            "would migrate {} to schema {CTX_SCHEMA} (runtime.default = {to})",
            path.display()
        )?;
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        state::create_private_dir_all(parent)?;
    }
    state::write_private(&backup, &text)?;
    state::write_private(path, &updated)?;
    state::write_private(
        &marker,
        &format!("schema = {CTX_SCHEMA}\nbackup = {:?}\n", backup.display()),
    )?;
    writeln!(
        w,
        "migrated {} to schema {CTX_SCHEMA} (runtime.default = {to}); backup at {}\n\
         downgrade with `zirv ctx config migrate --downgrade`",
        path.display(),
        backup.display()
    )?;
    Ok(0)
}

fn key_parts(key: &str) -> CtxResult<Vec<toml_edit::Key>> {
    toml_edit::Key::parse(key).map_err(|e| format!("invalid config key {key:?}: {e}").into())
}

fn slot<'a>(item: &'a mut Item, parts: &[toml_edit::Key]) -> CtxResult<&'a mut Item> {
    let Some((key, rest)) = parts.split_first() else {
        return Ok(item);
    };
    if item.is_none() {
        *item = Item::Table(toml_edit::Table::new());
    }
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| format!("config key {key}: parent is not a table"))?;
    slot(table.entry(key.get()).or_insert(Item::None), rest)
}

fn semantic_value(value: &Value) -> CtxResult<toml::Value> {
    let mut value = value.clone();
    value.decor_mut().clear();
    let table: toml::Table = toml::from_str(&format!("value = {value}"))?;
    Ok(table["value"].clone())
}

fn edit(doc: &mut DocumentMut, key: &str, raw: &str, append: bool) -> CtxResult<bool> {
    let mut value = raw.parse::<Value>().unwrap_or_else(|_| Value::from(raw));
    let target = slot(doc.as_item_mut(), &key_parts(key)?)?;
    if append {
        if target.is_none() {
            *target = Item::Value(Value::Array(toml_edit::Array::new()));
        }
        let array = target
            .as_array_mut()
            .ok_or_else(|| format!("config key {key} is not an array"))?;
        let incoming = semantic_value(&value)?;
        for existing in array.iter() {
            if semantic_value(existing)? == incoming {
                return Ok(false);
            }
        }
        // Comments after the final comma belong to the array's trailing
        // decoration. Keep them before the new element, beside the old one.
        if (array.trailing_comma() || array.is_empty())
            && let Some((before_close, closing_indent)) = array
                .trailing()
                .as_str()
                .unwrap_or_default()
                .rsplit_once('\n')
        {
            let indent = array
                .iter()
                .last()
                .and_then(|v| v.decor().prefix())
                .and_then(|prefix| prefix.as_str())
                .and_then(|prefix| prefix.rsplit_once('\n'))
                .map(|(_, indent)| indent)
                .unwrap_or("  ");
            value
                .decor_mut()
                .set_prefix(format!("{before_close}\n{indent}"));
            let trailing = format!("\n{closing_indent}");
            array.set_trailing(trailing);
        }
        array.push_formatted(value);
    } else {
        if let Some(old) = target.as_value() {
            *value.decor_mut() = old.decor().clone();
        }
        *target = Item::Value(value);
    }
    Ok(true)
}

pub fn run(args: &ConfigArgs, w: &mut dyn Write) -> CtxResult<i32> {
    let path = config::operator_path()?;
    if let ConfigCommand::Migrate {
        to,
        downgrade,
        dry_run,
    } = &args.command
    {
        return migrate(&path, to, *downgrade, *dry_run, w);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let (key, value, append) = match &args.command {
        ConfigCommand::Show { key: None } => {
            write!(w, "{text}")?;
            return Ok(0);
        }
        ConfigCommand::Show { key: Some(key) } => {
            let doc: DocumentMut = text.parse()?;
            let mut item = doc.as_item();
            for part in key_parts(key)? {
                item = item
                    .get(part.get())
                    .ok_or_else(|| format!("config key {key} is not set"))?;
            }
            writeln!(w, "{item}")?;
            return Ok(0);
        }
        ConfigCommand::Set { key, value } => (key, value, false),
        ConfigCommand::Add { key, value } => (key, value, true),
        // Handled above, before the document is even read.
        ConfigCommand::Migrate { .. } => unreachable!(),
    };
    let mut doc: DocumentMut = text.parse()?;
    let changed = edit(&mut doc, key, value, append)?;
    let updated = doc.to_string();
    config::validate_operator_document(&updated)
        .map_err(|e| format!("refusing to update {}: {e}", path.display()))?;
    if !changed {
        writeln!(w, "{key}: element already present (unchanged)")?;
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        state::create_private_dir_all(parent)?;
    }
    state::write_private(&path, &updated)?;
    writeln!(w, "{key}: {}", if append { "added" } else { "set" })?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::testenv::HomeGuard;
    use super::*;

    fn invoke(command: ConfigCommand) -> CtxResult<String> {
        let mut output = Vec::new();
        assert_eq!(run(&ConfigArgs { command }, &mut output)?, 0);
        Ok(String::from_utf8(output)?)
    }

    fn set(key: &str, value: &str) -> ConfigCommand {
        ConfigCommand::Set {
            key: key.into(),
            value: value.into(),
        }
    }

    fn add(key: &str, value: &str) -> ConfigCommand {
        ConfigCommand::Add {
            key: key.into(),
            value: value.into(),
        }
    }

    fn migrate_cmd(downgrade: bool) -> ConfigCommand {
        ConfigCommand::Migrate {
            to: "native".into(),
            downgrade,
            dry_run: false,
        }
    }

    /// Issue #491: running the migration twice must be a no-op the second
    /// time -- same bytes in `ctx.toml`, same backup, and a message that says
    /// so rather than a second backup that has silently overwritten the
    /// operator's real pre-migration document with the migrated one.
    #[test]
    fn migrating_twice_leaves_the_second_run_with_nothing_to_do() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let path = config::operator_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A comment, so the comment-preserving promise is under test too.
        std::fs::write(&path, "# my notes\n[score]\nwindow = 3\n").unwrap();

        let first = invoke(migrate_cmd(false)).unwrap();
        assert!(first.contains("schema 2"), "{first}");
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("# my notes"), "{migrated}");
        assert!(migrated.contains("default = \"native\""), "{migrated}");
        let backup = super::backup_path(&path);
        let backed_up = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(backed_up, "# my notes\n[score]\nwindow = 3\n");

        let second = invoke(migrate_cmd(false)).unwrap();
        assert!(second.contains("unchanged"), "{second}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), backed_up);
    }

    /// The rollback half of the same promise: the document that predates the
    /// migration comes back byte for byte, and native state on disk is not
    /// part of the transaction at all.
    #[test]
    fn a_downgrade_restores_the_pre_migration_document_and_touches_no_native_state() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let path = config::operator_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "# my notes\n[score]\nwindow = 3\n";
        std::fs::write(&path, original).unwrap();
        // Native state that must survive a round trip in either direction:
        // the provider config, and a journal naming the harness conversation
        // a native session took over from.
        let native = path.with_file_name("native.toml");
        std::fs::write(&native, "schema=1\n").unwrap();
        let journal = path.with_file_name("journal.jsonl");
        std::fs::write(&journal, "{\"harness_session\":\"abc-123\"}\n").unwrap();

        invoke(migrate_cmd(false)).unwrap();
        let back = invoke(migrate_cmd(true)).unwrap();
        assert!(back.contains("schema 1"), "{back}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&native).unwrap(), "schema=1\n");
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap(),
            "{\"harness_session\":\"abc-123\"}\n"
        );
        // Idempotent downwards too.
        let again = invoke(migrate_cmd(true)).unwrap();
        assert!(again.contains("unchanged"), "{again}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// With no backup to restore -- an operator who hand-edited the table in
    /// -- the downgrade still has to leave a document an older binary can
    /// parse, which means the whole `[runtime]` table goes.
    #[test]
    fn a_downgrade_without_a_backup_removes_the_runtime_table() {
        let stripped =
            strip_runtime_table("[score]\nwindow = 3\n\n[runtime]\ndefault = \"native\"\n")
                .unwrap()
                .expect("a runtime table to remove");
        assert!(!stripped.contains("runtime"), "{stripped}");
        let table: toml::Table = toml::from_str(&stripped).unwrap();
        assert_eq!(table["score"]["window"].as_integer(), Some(3));
        assert_eq!(strip_runtime_table("[score]\nwindow = 3\n").unwrap(), None);
    }

    #[test]
    fn an_unknown_migration_target_is_refused_before_anything_is_written() {
        let error = migrate_document("", "natve").unwrap_err().to_string();
        assert!(error.contains("expected `harness` or `native`"), "{error}");
    }

    #[test]
    fn cli_accepts_negative_toml_values() {
        use clap::Parser;
        for verb in ["set", "add"] {
            let cli = super::super::CtxCli::try_parse_from([
                "zirv ctx",
                "config",
                verb,
                "score.window",
                "-3",
            ])
            .unwrap();
            assert!(matches!(cli.verb, super::super::CtxVerb::Config(_)));
        }
    }

    #[test]
    fn creates_operator_file_and_parses_typed_values_and_strings() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        invoke(set("worker.codex", "gpt-5")).unwrap();
        invoke(set("memory.enabled", "true")).unwrap();
        invoke(set("score.window", "3")).unwrap();
        invoke(set("dash.workdir_roots", r#"["/tmp/a", "/tmp/b"]"#)).unwrap();
        let text = invoke(ConfigCommand::Show { key: None }).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["worker"]["codex"].as_str(), Some("gpt-5"));
        assert_eq!(table["memory"]["enabled"].as_bool(), Some(true));
        assert_eq!(table["score"]["window"].as_integer(), Some(3));
        assert_eq!(table["dash"]["workdir_roots"].as_array().unwrap().len(), 2);
        assert_eq!(
            invoke(ConfigCommand::Show {
                key: Some("score.window".into())
            })
            .unwrap()
            .trim(),
            "3"
        );
    }

    #[test]
    fn preserves_comments_and_formatting_and_add_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let original = "# my config\n[worker] # models\ncodex  = 'old' # keep\n\n[dash]\nworkdir_roots = [\n  '/tmp/a', # first\n]\n";
        let path = config::operator_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();
        invoke(set("worker.codex", "new")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, original.replace("'old'", "\"new\""));
        assert!(
            invoke(add("dash.workdir_roots", "\"/tmp/a\""))
                .unwrap()
                .contains("already present")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        invoke(add("dash.workdir_roots", "/tmp/b")).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("'/tmp/a', # first")
        );
        invoke(add("safety.allow", "cargo test")).unwrap();
        assert!(
            invoke(add("safety.allow", "cargo test"))
                .unwrap()
                .contains("already present")
        );
    }

    #[test]
    fn invalid_edits_leave_file_untouched_including_separate_policy_sections() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        invoke(set("memory.enabled", "true")).unwrap();
        let path = config::operator_path().unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        for command in [
            set("unknown", "true"),
            set("dash.unknown", "3"),
            set("memory.enabled", "nope"),
            set("safety.unknown", "true"),
            set("policy.unknown", "true"),
            set("safety.allow", "3"),
            set("policy.network", "3"),
            add("memory.enabled", "true"),
            set("memory.enabled.child", "true"),
            set("", "3"),
        ] {
            assert!(invoke(command).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn missing_show_and_invalid_new_config_create_nothing() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        assert_eq!(invoke(ConfigCommand::Show { key: None }).unwrap(), "");
        assert!(
            invoke(ConfigCommand::Show {
                key: Some("worker.codex".into())
            })
            .is_err()
        );
        assert!(invoke(set("worker.codex", "3")).is_err());
        assert!(!home.path().join(".zirv").exists());
    }

    #[test]
    fn edits_inline_tables_and_refuses_malformed_existing_document() {
        let home = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(home.path());
        let path = config::operator_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "worker = { codex = 'old' } # keep\n").unwrap();
        invoke(set("worker.codex", "new")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "worker = { codex = \"new\" } # keep\n"
        );
        std::fs::write(&path, "[broken").unwrap();
        assert!(invoke(set("worker.codex", "new")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken");
    }
}
