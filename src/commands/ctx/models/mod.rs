//! Account-local model discovery and public price-cache I/O.
//!
//! The catalogue and price modules stay deterministic: this module reads the
//! Codex-owned cache and Claude transcripts, persists observations, and
//! injects the resulting rows into their pure lookup helpers.

pub(crate) mod scorecard;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::time::Duration;

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::catalogue::{self, DiscoveredModel, Tier};
use super::config::{CtxConfig, ModelsConfig, env_from_process};
use super::price::{self, ModelPrice, PriceTable};
use super::state::{self, StateDir};

const REGISTRY_FILE: &str = "models.json";
const PRICE_CACHE_FILE: &str = "prices.json";
#[cfg(not(test))]
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
#[cfg(not(test))]
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
#[cfg(not(test))]
const MAX_REMOTE_BYTES: u64 = 32 * 1024 * 1024;
const REFRESH_LOCK_FILE: &str = "models-refresh.lock";
const REFRESH_ATTEMPT_FILE: &str = "models-refresh-attempt";
const REGISTRY_MAX_AGE_SECS: u64 = 24 * 3600;
const PRICE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
const REFRESH_BACKOFF_SECS: u64 = 3600;
const MAX_TRANSCRIPT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TRANSCRIPTS: usize = 256;

#[derive(Debug, Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    command: Option<ModelsCommand>,
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Refresh account-local discovery and the public price cache.
    Refresh {
        /// Print nothing; used by the automatic background refresher.
        #[arg(long)]
        quiet: bool,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Registry {
    pub updated_at: u64,
    pub models: BTreeMap<String, RegistryModel>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RegistryModel {
    pub vendor: String,
    pub id: String,
    pub aliases: Vec<String>,
    pub family: Option<String>,
    pub version: Option<String>,
    pub context_window: Option<u64>,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub available: bool,
    pub visibility: Option<String>,
    pub priority: Option<i64>,
    pub supported_in_api: Option<bool>,
    pub first_seen: u64,
    pub last_seen: u64,
    pub retirement_at: Option<u64>,
    pub upgrade: Option<String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PriceCache {
    fetched_at: u64,
    models_dev_etag: Option<String>,
    litellm_etag: Option<String>,
    models_dev: BTreeMap<String, ModelPrice>,
    litellm: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PriceProvenance {
    pub source: String,
    pub approximate: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectivePrices {
    pub table: PriceTable,
    pub provenance: BTreeMap<String, PriceProvenance>,
}

#[derive(Debug, Deserialize)]
struct CodexCache {
    models: Vec<CodexModel>,
}

#[derive(Debug, Deserialize)]
struct CodexModel {
    slug: String,
    #[serde(default)]
    visibility: String,
    priority: Option<i64>,
    context_window: Option<u64>,
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexEffort>,
    upgrade: Option<CodexUpgrade>,
    supported_in_api: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CodexEffort {
    effort: String,
}

#[derive(Debug, Deserialize)]
struct CodexUpgrade {
    model: String,
    retirement_at: Option<String>,
}

pub fn run(args: &ModelsArgs, w: &mut dyn Write) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, &env_from_process())?;
    let state = StateDir::resolve(&env_from_process())?;
    if let Some(ModelsCommand::Refresh { quiet }) = &args.command {
        // At most one refresher runs; a busy lock means another is already working.
        let _ = std::fs::create_dir_all(state.root());
        let Ok(_lock) = state::try_acquire_lock(&state.root().join(REFRESH_LOCK_FILE)) else {
            return Ok(0);
        };
        refresh(&cfg, &state, state::now_secs())?;
        if *quiet {
            return Ok(0);
        }
    }
    let mut registry = load_registry(&state);
    if args.command.is_none() && cfg.models.discovery {
        registry = discover_local(registry, state::now_secs());
    }
    let prices = effective_prices(&cfg, &state);
    let rows = list_rows(&cfg, &registry, &prices);
    let card = scorecard::build(&state, &prices.table, state::now_secs());
    let warnings = avoid_warnings(&cfg, &registry);
    if args.json {
        let report = serde_json::json!({
            "models": rows,
            "scorecard": card,
            "avoid": {
                "manual": cfg.models.avoid,
                "auto_enabled": cfg.models.auto_avoid,
                "auto": if cfg.models.auto_avoid { card.auto_avoid_ids() } else { BTreeSet::new() },
                "warnings": warnings,
            },
        });
        serde_json::to_writer_pretty(&mut *w, &report)?;
        writeln!(w)?;
    } else {
        writeln!(
            w,
            "VENDOR\tID\tFAMILY\tRUNG\tTIER\tAVAILABLE\tPRICE IN/OUT\tSOURCE\tRETIREMENT"
        )?;
        for row in rows {
            let marker = if row.approximate { "~" } else { "" };
            writeln!(
                w,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}{}/{}\t{}\t{}",
                row.vendor,
                row.id,
                row.family.as_deref().unwrap_or("-"),
                row.rung.as_deref().unwrap_or("-"),
                row.tier.as_deref().unwrap_or("-"),
                row.availability,
                marker,
                row.input_micros_per_million
                    .map(format_dollars)
                    .unwrap_or_else(|| "?".into()),
                row.output_micros_per_million
                    .map(format_dollars)
                    .unwrap_or_else(|| "?".into()),
                row.price_source.as_deref().unwrap_or("-"),
                row.retirement_at
                    .map(format_epoch_date)
                    .unwrap_or_else(|| "-".into()),
            )?;
        }
        for warning in &warnings {
            writeln!(w, "warning: {warning}")?;
        }
        scorecard::render(&card, w)?;
    }
    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ModelRow {
    vendor: String,
    id: String,
    family: Option<String>,
    rung: Option<String>,
    tier: Option<String>,
    availability: String,
    input_micros_per_million: Option<u64>,
    output_micros_per_million: Option<u64>,
    price_source: Option<String>,
    approximate: bool,
    retirement_at: Option<u64>,
    upgrade: Option<String>,
}

fn list_rows(cfg: &CtxConfig, registry: &Registry, prices: &EffectivePrices) -> Vec<ModelRow> {
    let mut ids: BTreeMap<(String, String), Option<&RegistryModel>> = BTreeMap::new();
    for model in registry.models.values() {
        ids.insert((model.vendor.clone(), model.id.clone()), Some(model));
    }
    for vendor in catalogue::vendors() {
        for rung in vendor.rungs {
            ids.entry((vendor.slug.to_string(), rung.id.to_string()))
                .or_insert(None);
        }
        for (id, _) in vendor.extra_prices {
            ids.entry((vendor.slug.to_string(), (*id).to_string()))
                .or_insert(None);
        }
    }
    let discovered = discovered_models(registry);
    let pins = effective_pins(cfg);
    let ladders: BTreeMap<String, Vec<catalogue::ResolvedRung>> = catalogue::vendors()
        .iter()
        .map(|vendor| {
            (
                vendor.slug.to_string(),
                catalogue::resolved_ladder(vendor, &discovered, &pins),
            )
        })
        .collect();
    ids.into_iter()
        .map(|((vendor, id), model)| {
            let normalized = catalogue::normalize_id(&id).to_lowercase();
            let ladder = ladders.get(&vendor);
            let rung = ladder.and_then(|rows| rows.iter().find(|r| r.id == normalized));
            let price = prices.table.models.get(&normalized);
            let provenance = prices.provenance.get(&normalized);
            ModelRow {
                family: model
                    .and_then(|m| m.family.clone())
                    .or_else(|| catalogue::model_family(&vendor, &id).map(str::to_string)),
                vendor,
                id,
                rung: rung.map(|r| r.alias.clone()),
                tier: rung.and_then(|r| r.tier).map(tier_name).map(str::to_string),
                availability: model
                    .map_or(
                        "snapshot",
                        |m| if m.available { "available" } else { "hidden" },
                    )
                    .to_string(),
                input_micros_per_million: price.map(|p| p.input_micros),
                output_micros_per_million: price.map(|p| p.output_micros),
                price_source: provenance.map(|p| p.source.clone()),
                approximate: provenance.is_some_and(|p| p.approximate),
                retirement_at: model.and_then(|m| m.retirement_at),
                upgrade: model.and_then(|m| m.upgrade.clone()),
            }
        })
        .collect()
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Cheap => "cheap",
        Tier::Standard => "standard",
        Tier::Deep => "deep",
    }
}

fn format_dollars(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let fraction = micros % 1_000_000;
    if fraction == 0 {
        format!("${whole}")
    } else {
        format!("${whole}.{fraction:06}")
            .trim_end_matches('0')
            .to_string()
    }
}

fn format_epoch_date(epoch: u64) -> String {
    let z = i64::try_from(epoch / 86_400).unwrap_or(i64::MAX) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn model_version(vendor: &str, id: &str) -> Option<String> {
    let normalized = catalogue::normalize_id(id).to_lowercase();
    match vendor {
        "anthropic" => {
            let family = catalogue::model_family(vendor, &normalized)?;
            normalized
                .strip_prefix(&format!("claude-{family}-"))
                .map(str::to_string)
        }
        "openai" => normalized
            .strip_prefix("gpt-")?
            .rsplit_once('-')
            .map(|(version, _)| version.to_string()),
        _ => None,
    }
}

pub(crate) fn load_registry(state: &StateDir) -> Registry {
    read_json(&state.root().join(REGISTRY_FILE))
        .filter(valid_registry)
        .unwrap_or_default()
}

fn valid_registry(registry: &Registry) -> bool {
    registry.models.iter().all(|(key, model)| {
        !model.vendor.is_empty()
            && !model.id.is_empty()
            && key == &format!("{}:{}", model.vendor, model.id)
            && model.first_seen <= model.last_seen
            && model
                .context_window
                .is_none_or(|window| (8_192..=10_000_000).contains(&window))
    })
}

fn load_price_cache(state: &StateDir) -> PriceCache {
    read_json(&state.root().join(PRICE_CACHE_FILE)).unwrap_or_default()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> CtxResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    state::write_private(path, &serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn refresh(cfg: &CtxConfig, state: &StateDir, now: u64) -> CtxResult<()> {
    if cfg.models.discovery {
        let mut registry = discover_local(load_registry(state), now);
        registry.updated_at = now;
        write_json(&state.root().join(REGISTRY_FILE), &registry)?;
    }
    if cfg.models.price_fetch {
        refresh_price_cache(state, now)?;
    }
    scorecard::refresh(state, &effective_prices(cfg, state).table, now)
}

fn discover_local(mut registry: Registry, now: u64) -> Registry {
    if let Some(path) = codex_cache_path()
        && let Ok(text) = std::fs::read_to_string(path)
    {
        for model in registry.models.values_mut() {
            if model.sources.iter().any(|source| source == "codex-cache") {
                model.available = false;
            }
        }
        merge_observations(&mut registry, parse_codex_cache(&text, now), now);
    }
    if let Some(root) = claude_projects_path() {
        merge_observations(&mut registry, scan_claude_transcripts(&root, now), now);
    }
    registry
}

fn codex_cache_path() -> Option<PathBuf> {
    crate::utils::home_dir()
        .ok()
        .map(|home| home.join(".codex/models_cache.json"))
}

fn claude_projects_path() -> Option<PathBuf> {
    crate::utils::home_dir()
        .ok()
        .map(|home| home.join(".claude/projects"))
}

fn parse_codex_cache(text: &str, now: u64) -> Vec<RegistryModel> {
    let Ok(cache) = serde_json::from_str::<CodexCache>(text) else {
        return Vec::new();
    };
    cache
        .models
        .into_iter()
        .map(|model| {
            let normalized = catalogue::normalize_id(&model.slug).to_lowercase();
            let family = catalogue::model_family("openai", &normalized);
            let version = model_version("openai", &normalized);
            RegistryModel {
                vendor: "openai".to_string(),
                id: normalized,
                family: family.map(str::to_string),
                version,
                context_window: model.context_window,
                efforts: model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|e| e.effort)
                    .collect(),
                default_effort: model.default_reasoning_level,
                available: model.visibility == "list",
                visibility: Some(model.visibility),
                priority: model.priority,
                supported_in_api: model.supported_in_api,
                first_seen: now,
                last_seen: now,
                retirement_at: model
                    .upgrade
                    .as_ref()
                    .and_then(|u| u.retirement_at.as_deref())
                    .and_then(super::window::parse_rfc3339_utc),
                upgrade: model.upgrade.map(|u| u.model),
                sources: vec!["codex-cache".to_string()],
                ..RegistryModel::default()
            }
        })
        .collect()
}

fn parse_claude_transcript(text: &str, now: u64) -> Vec<RegistryModel> {
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(model) = value
            .get("message")
            .and_then(|message| message.get("model"))
            .and_then(Value::as_str)
        {
            ids.insert(catalogue::normalize_id(model).to_lowercase());
        }
    }
    ids.into_iter()
        .map(|id| {
            let family = catalogue::model_family("anthropic", &id);
            RegistryModel {
                vendor: "anthropic".to_string(),
                aliases: family.into_iter().map(str::to_string).collect(),
                family: family.map(str::to_string),
                version: model_version("anthropic", &id),
                id,
                available: true,
                first_seen: now,
                last_seen: now,
                sources: vec!["claude-transcript".to_string()],
                ..RegistryModel::default()
            }
        })
        .collect()
}

fn scan_claude_transcripts(root: &Path, now: u64) -> Vec<RegistryModel> {
    let mut files = Vec::new();
    collect_jsonl(root, &mut files);
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let mut observations = Vec::new();
    for (_, path) in files.into_iter().take(MAX_TRANSCRIPTS) {
        if let Some(text) = read_transcript_tail(&path) {
            observations.extend(parse_claude_transcript(&text, now));
        }
    }
    observations
}

/// The last `MAX_TRANSCRIPT_BYTES` of a transcript as text. A seek can land inside a multibyte
/// character, so the bytes are decoded lossily and the first (partial) line is dropped.
fn read_transcript_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let truncated = len > MAX_TRANSCRIPT_BYTES;
    if truncated {
        file.seek(std::io::SeekFrom::Start(len - MAX_TRANSCRIPT_BYTES))
            .ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if !truncated {
        return Some(text.into_owned());
    }
    Some(
        text.split_once('\n')
            .map_or("", |(_, rest)| rest)
            .to_string(),
    )
}

fn collect_jsonl(root: &Path, files: &mut Vec<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            files.push((modified, path));
        }
    }
}

fn merge_observations(registry: &mut Registry, observations: Vec<RegistryModel>, now: u64) {
    for mut observed in observations {
        let key = format!("{}:{}", observed.vendor, observed.id);
        if let Some(existing) = registry.models.get_mut(&key) {
            let first_seen = existing.first_seen;
            for source in &existing.sources {
                if !observed.sources.contains(source) {
                    observed.sources.push(source.clone());
                }
            }
            observed.first_seen = first_seen;
            observed.last_seen = now;
            *existing = observed;
        } else {
            registry.models.insert(key, observed);
        }
    }
}

fn discovered_models(registry: &Registry) -> Vec<DiscoveredModel> {
    registry
        .models
        .values()
        .map(|m| DiscoveredModel::new(&m.vendor, &m.id, m.available))
        .collect()
}

fn effective_pins(cfg: &CtxConfig) -> BTreeMap<String, String> {
    let mut pins = cfg.models.pin.clone();
    let mut add = |vendor: &str, model: &Option<String>| {
        if let Some(model) = model
            && let Some(family) = catalogue::model_family(vendor, model)
        {
            pins.entry(format!("{vendor}.{family}"))
                .or_insert_with(|| model.clone());
        }
    };
    add("anthropic", &cfg.worker.claude);
    add("openai", &cfg.worker.codex);
    add("anthropic", &cfg.review.claude);
    add("openai", &cfg.review.codex);
    for model in [
        &cfg.handover.claude.cheap,
        &cfg.handover.claude.standard,
        &cfg.handover.claude.deep,
    ] {
        add("anthropic", model);
    }
    for model in [
        &cfg.handover.codex.cheap,
        &cfg.handover.codex.standard,
        &cfg.handover.codex.deep,
    ] {
        add("openai", model);
    }
    for model in [
        &cfg.model_tiers.claude.fast,
        &cfg.model_tiers.claude.standard,
        &cfg.model_tiers.claude.deep,
    ] {
        add("anthropic", model);
    }
    for model in [
        &cfg.model_tiers.codex.fast,
        &cfg.model_tiers.codex.standard,
        &cfg.model_tiers.codex.deep,
    ] {
        add("openai", model);
    }
    if let Some(target) = &cfg.endpoint.claude {
        add("anthropic", &target.model);
    }
    if let Some(target) = &cfg.endpoint.codex {
        add("openai", &target.model);
    }
    pins
}

fn parse_models_dev(text: &str) -> BTreeMap<String, ModelPrice> {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return BTreeMap::new();
    };
    let mut prices = BTreeMap::new();
    for provider in ["anthropic", "openai"] {
        let Some(models) = root
            .get(provider)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (id, model) in models {
            let Some(cost) = model.get("cost") else {
                continue;
            };
            let Some(input) = json_decimal_to_micros(cost.get("input"), 1_000_000.0) else {
                continue;
            };
            let Some(output) = json_decimal_to_micros(cost.get("output"), 1_000_000.0) else {
                continue;
            };
            let cache_read =
                json_decimal_to_micros(cost.get("cache_read"), 1_000_000.0).unwrap_or(input);
            let cache_write =
                json_decimal_to_micros(cost.get("cache_write"), 1_000_000.0).unwrap_or(input);
            prices.insert(
                catalogue::normalize_id(id).to_lowercase(),
                ModelPrice {
                    input_micros: input,
                    output_micros: output,
                    cache_read_micros: cache_read,
                    cache_write_micros: cache_write,
                },
            );
        }
    }
    prices
}

fn parse_litellm(text: &str) -> BTreeMap<String, ModelPrice> {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return BTreeMap::new();
    };
    let Some(entries) = root.as_object() else {
        return BTreeMap::new();
    };
    entries
        .iter()
        .filter(|(id, model)| {
            !id.contains('/')
                && matches!(
                    model.get("litellm_provider").and_then(Value::as_str),
                    Some("anthropic" | "openai")
                )
        })
        .filter_map(|(id, model)| {
            let input =
                json_decimal_to_micros(model.get("input_cost_per_token"), 1_000_000_000_000.0)?;
            let output =
                json_decimal_to_micros(model.get("output_cost_per_token"), 1_000_000_000_000.0)?;
            let cache_read = json_decimal_to_micros(
                model.get("cache_read_input_token_cost"),
                1_000_000_000_000.0,
            )
            .unwrap_or(input);
            let cache_write = json_decimal_to_micros(
                model.get("cache_creation_input_token_cost"),
                1_000_000_000_000.0,
            )
            .unwrap_or(input);
            Some((
                catalogue::normalize_id(id).to_lowercase(),
                ModelPrice {
                    input_micros: input,
                    output_micros: output,
                    cache_read_micros: cache_read,
                    cache_write_micros: cache_write,
                },
            ))
        })
        .collect()
}

fn json_decimal_to_micros(value: Option<&Value>, scale: f64) -> Option<u64> {
    let number = value?.as_f64()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    Some((number * scale).round().min(u64::MAX as f64) as u64)
}

fn effective_prices_from(
    cache: &PriceCache,
    override_table: Option<PriceTable>,
) -> EffectivePrices {
    let mut table = price::built_in_table();
    let mut provenance: BTreeMap<String, PriceProvenance> = table
        .models
        .keys()
        .map(|id| {
            (
                id.clone(),
                PriceProvenance {
                    source: "snapshot".into(),
                    approximate: id == "gpt-5.6-sol",
                },
            )
        })
        .collect();
    for (id, rate) in &cache.models_dev {
        table.models.insert(id.clone(), *rate);
        provenance.insert(
            id.clone(),
            PriceProvenance {
                source: "models.dev".into(),
                approximate: cache.litellm.get(id).is_some_and(|other| other != rate),
            },
        );
    }
    if cache.fetched_at != 0 {
        table.as_of = format_epoch_date(cache.fetched_at);
    }
    if let Some(overrides) = override_table {
        table.as_of = overrides.as_of;
        for (id, rate) in overrides.models {
            let id = catalogue::normalize_id(&id).to_lowercase();
            table.models.insert(id.clone(), rate);
            provenance.insert(
                id,
                PriceProvenance {
                    source: "operator".into(),
                    approximate: false,
                },
            );
        }
    }
    EffectivePrices { table, provenance }
}

pub(crate) fn effective_prices(cfg: &CtxConfig, state: &StateDir) -> EffectivePrices {
    let cache = load_price_cache(state);
    effective_prices_from(&cache, load_override(cfg))
}

pub(crate) fn resolve_price_table(cfg: &CtxConfig) -> PriceTable {
    StateDir::resolve(&env_from_process())
        .map(|state| effective_prices(cfg, &state).table)
        .unwrap_or_else(|_| effective_prices_from(&PriceCache::default(), load_override(cfg)).table)
}

fn load_override(cfg: &CtxConfig) -> Option<PriceTable> {
    let path = match &cfg.price.table_path {
        Some(path) => expand_home(path),
        None => crate::utils::home_dir().ok()?.join(".zirv/prices.toml"),
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = crate::utils::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(not(test))]
fn refresh_price_cache(state: &StateDir, now: u64) -> CtxResult<()> {
    let mut cache = load_price_cache(state);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(2)
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .into();
    let mut reached_source = false;
    if let Ok((models_dev, models_dev_etag, models_dev_changed)) =
        fetch_json(&agent, MODELS_DEV_URL, cache.models_dev_etag.as_deref())
    {
        reached_source = true;
        if models_dev_changed {
            let parsed = parse_models_dev(&models_dev);
            if !parsed.is_empty() {
                cache.models_dev = parsed;
                cache.models_dev_etag = models_dev_etag;
            }
        }
    }
    if let Ok((litellm, litellm_etag, litellm_changed)) =
        fetch_json(&agent, LITELLM_URL, cache.litellm_etag.as_deref())
    {
        reached_source = true;
        if litellm_changed {
            let parsed = parse_litellm(&litellm);
            if !parsed.is_empty() {
                cache.litellm = parsed;
                cache.litellm_etag = litellm_etag;
            }
        }
    }
    if reached_source {
        cache.fetched_at = now;
        write_json(&state.root().join(PRICE_CACHE_FILE), &cache)?;
    }
    Ok(())
}

#[cfg(test)]
fn refresh_price_cache(_state: &StateDir, _now: u64) -> CtxResult<()> {
    Err("network refresh is disabled in tests".into())
}

#[cfg(not(test))]
fn fetch_json(
    agent: &ureq::Agent,
    url: &str,
    etag: Option<&str>,
) -> CtxResult<(String, Option<String>, bool)> {
    let mut request = agent
        .get(url)
        .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")));
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    let mut response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(304)) => {
            return Ok((String::new(), etag.map(str::to_string), false));
        }
        Err(error) => return Err(format!("price refresh failed for {url}: {error}").into()),
    };
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_REMOTE_BYTES)
        .read_to_string()?;
    Ok((body, etag, true))
}

pub(crate) fn retirement_warnings(cfg: &CtxConfig, registry: &Registry, now: u64) -> Vec<String> {
    let discovered = discovered_models(registry);
    let pins = effective_pins(cfg);
    let mut selected = BTreeSet::new();
    for vendor in ["anthropic", "openai"] {
        let Some(vendor_entry) = catalogue::vendor(vendor) else {
            continue;
        };
        for rung in catalogue::resolved_ladder(vendor_entry, &discovered, &pins) {
            selected.insert((vendor.to_string(), rung.id));
        }
    }
    let mut select = |vendor: &str, model: Option<&String>| {
        if let Some(model) = model {
            selected.insert((
                vendor.to_string(),
                catalogue::normalize_id(model).to_lowercase(),
            ));
        }
    };
    select("anthropic", cfg.worker.claude.as_ref());
    select("openai", cfg.worker.codex.as_ref());
    select("anthropic", cfg.review.claude.as_ref());
    select("openai", cfg.review.codex.as_ref());
    for model in [
        cfg.handover.claude.cheap.as_ref(),
        cfg.handover.claude.standard.as_ref(),
        cfg.handover.claude.deep.as_ref(),
    ] {
        select("anthropic", model);
    }
    for model in [
        cfg.handover.codex.cheap.as_ref(),
        cfg.handover.codex.standard.as_ref(),
        cfg.handover.codex.deep.as_ref(),
    ] {
        select("openai", model);
    }
    for model in [
        cfg.model_tiers.claude.fast.as_ref(),
        cfg.model_tiers.claude.standard.as_ref(),
        cfg.model_tiers.claude.deep.as_ref(),
    ] {
        select("anthropic", model);
    }
    for model in [
        cfg.model_tiers.codex.fast.as_ref(),
        cfg.model_tiers.codex.standard.as_ref(),
        cfg.model_tiers.codex.deep.as_ref(),
    ] {
        select("openai", model);
    }
    for (key, model) in &cfg.models.pin {
        if let Some((vendor, _)) = key.split_once('.') {
            select(vendor, Some(model));
        }
    }
    if let Some(target) = &cfg.endpoint.claude {
        select("anthropic", target.model.as_ref());
    }
    if let Some(target) = &cfg.endpoint.codex {
        select("openai", target.model.as_ref());
    }
    let cutoff = now.saturating_add(14 * 86_400);
    registry
        .models
        .values()
        .filter(|model| {
            selected.contains(&(model.vendor.clone(), model.id.clone()))
                && model
                    .retirement_at
                    .is_some_and(|at| at >= now && at <= cutoff)
        })
        .map(|model| {
            format!(
                "model {} retires within 14 days{}",
                model.id,
                model
                    .upgrade
                    .as_deref()
                    .map(|target| format!("; upgrade to {target}"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

#[cfg(not(test))]
fn with_discovered<R>(f: impl FnOnce(&[DiscoveredModel]) -> R) -> R {
    static DISCOVERED: std::sync::OnceLock<Vec<DiscoveredModel>> = std::sync::OnceLock::new();
    let models = DISCOVERED.get_or_init(|| {
        let env = env_from_process();
        if env("ZIRV_CTX_MODELS_DISCOVERY").is_some_and(|v| v.eq_ignore_ascii_case("false")) {
            return Vec::new();
        }
        StateDir::resolve(&env)
            .map(|state| discovered_models(&load_registry(&state)))
            .unwrap_or_default()
    });
    f(models)
}

#[cfg(test)]
thread_local! {
    static TEST_DISCOVERED: std::cell::RefCell<Vec<DiscoveredModel>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn set_test_discovered(models: Vec<DiscoveredModel>) {
    TEST_DISCOVERED.with(|cell| *cell.borrow_mut() = models);
}

#[cfg(test)]
fn with_discovered<R>(f: impl FnOnce(&[DiscoveredModel]) -> R) -> R {
    TEST_DISCOVERED.with(|cell| f(&cell.borrow()))
}

/// Empty (the static ladder applies unchanged) unless discovery has a model for this vendor or
/// `avoid` must be applied to the static ladder.
fn ladder_with(
    vendor: &catalogue::Vendor,
    discovery: bool,
    pins: impl FnOnce() -> BTreeMap<String, String>,
    avoid: &BTreeSet<String>,
) -> Vec<catalogue::ResolvedRung> {
    with_discovered(|discovered| {
        let discovered = if discovery { discovered } else { &[] };
        if avoid.is_empty()
            && !discovered
                .iter()
                .any(|m| m.available && m.vendor == vendor.slug)
        {
            return Vec::new();
        }
        catalogue::resolved_ladder_avoiding(vendor, discovered, &pins(), avoid).0
    })
}

fn avoid_ids(ids: &[String]) -> BTreeSet<String> {
    ids.iter()
        .map(|id| catalogue::normalize_id(id).to_lowercase())
        .collect()
}

/// The operator's `[models]` table for callers without a config: `~/.zirv/ctx.toml` plus
/// `ZIRV_CTX_MODELS_*`, read once per process (repos cannot set `models.*`).
#[cfg(not(test))]
fn process_models() -> ModelsConfig {
    static MODELS: std::sync::OnceLock<ModelsConfig> = std::sync::OnceLock::new();
    MODELS
        .get_or_init(|| ModelsConfig::load_operator_only(&env_from_process()).unwrap_or_default())
        .clone()
}

/// The refresher's auto-avoid set from the cached scorecard, read once per process.
#[cfg(not(test))]
fn cached_auto_avoid() -> BTreeSet<String> {
    static AUTO: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    AUTO.get_or_init(|| {
        StateDir::resolve(&env_from_process())
            .ok()
            .and_then(|state| scorecard::load(&state))
            .map(|card| card.auto_avoid_ids())
            .unwrap_or_default()
    })
    .clone()
}

#[cfg(test)]
thread_local! {
    static TEST_MODELS: std::cell::RefCell<ModelsConfig> =
        std::cell::RefCell::new(ModelsConfig::default());
    static TEST_AUTO_AVOID: std::cell::RefCell<BTreeSet<String>> =
        const { std::cell::RefCell::new(BTreeSet::new()) };
}

#[cfg(test)]
pub(crate) fn set_test_avoid(ids: &[&str]) {
    TEST_MODELS.with(|cell| {
        cell.borrow_mut().avoid = ids.iter().map(|id| (*id).to_string()).collect();
    });
}

#[cfg(test)]
pub(crate) fn set_test_cached_auto_avoid(ids: &[&str]) {
    let ids: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
    TEST_AUTO_AVOID.with(|cell| *cell.borrow_mut() = avoid_ids(&ids));
}

#[cfg(test)]
fn process_models() -> ModelsConfig {
    TEST_MODELS.with(|cell| cell.borrow().clone())
}

#[cfg(test)]
fn cached_auto_avoid() -> BTreeSet<String> {
    TEST_AUTO_AVOID.with(|cell| cell.borrow().clone())
}

/// The ids a resolver must avoid under `cfg`: the operator's list, plus the refresher's
/// auto-avoid set only while `[models] auto_avoid` is on. Empty by default.
pub(crate) fn avoid_for(cfg: &CtxConfig) -> BTreeSet<String> {
    avoid_from(&cfg.models)
}

fn avoid_from(models: &ModelsConfig) -> BTreeSet<String> {
    let mut set = avoid_ids(&models.avoid);
    if models.auto_avoid {
        set.extend(cached_auto_avoid());
    }
    set
}

/// The same set read straight from `state`, for the dashboard badge. The scorecard file is
/// re-parsed only when its mtime changes.
pub(crate) fn avoid_for_state(cfg: &CtxConfig, state: &StateDir) -> BTreeSet<String> {
    let mut set = avoid_ids(&cfg.models.avoid);
    if !cfg.models.auto_avoid {
        return set;
    }
    static CACHE: std::sync::Mutex<Option<(std::time::SystemTime, BTreeSet<String>)>> =
        std::sync::Mutex::new(None);
    let modified = std::fs::metadata(scorecard_path(state))
        .and_then(|meta| meta.modified())
        .ok();
    let Ok(mut cache) = CACHE.lock() else {
        return set;
    };
    match (modified, cache.as_ref()) {
        (None, _) => *cache = None,
        (Some(at), Some((cached_at, ids))) if *cached_at == at => set.extend(ids.iter().cloned()),
        (Some(at), _) => {
            let ids = scorecard::load(state)
                .map(|card| card.auto_avoid_ids())
                .unwrap_or_default();
            set.extend(ids.iter().cloned());
            *cache = Some((at, ids));
        }
    }
    set
}

/// The cached scorecard's path, so a viewer can reload it only when the file changes.
pub(crate) fn scorecard_path(state: &StateDir) -> PathBuf {
    state.root().join(scorecard::SCORECARD_FILE)
}

/// The run-time ladder for callers without a config: discovered models only. Empty (the
/// static ladder applies unchanged) when the registry holds nothing for this vendor.
pub(crate) fn runtime_ladder(vendor: &catalogue::Vendor) -> Vec<catalogue::ResolvedRung> {
    let models = process_models();
    ladder_with(
        vendor,
        models.discovery,
        || models.pin.clone(),
        &avoid_from(&models),
    )
}

/// [`runtime_ladder`] honouring `[models] discovery` and the operator's explicit model
/// config and pins, which always win over a discovered id, and `[models] avoid`.
pub(crate) fn ladder_for(
    cfg: &CtxConfig,
    vendor: &catalogue::Vendor,
) -> Vec<catalogue::ResolvedRung> {
    ladder_with(
        vendor,
        cfg.models.discovery,
        || effective_pins(cfg),
        &avoid_for(cfg),
    )
}

/// [`ladder_for`] without avoid: for recognising which tier an already-running model is on.
pub(crate) fn identity_ladder_for(
    cfg: &CtxConfig,
    vendor: &catalogue::Vendor,
) -> Vec<catalogue::ResolvedRung> {
    ladder_with(
        vendor,
        cfg.models.discovery,
        || effective_pins(cfg),
        &BTreeSet::new(),
    )
}

/// What avoid did, or could not do, for `zirv ctx models`.
fn avoid_warnings(cfg: &CtxConfig, registry: &Registry) -> Vec<String> {
    let avoid = avoid_for(cfg);
    if avoid.is_empty() {
        return Vec::new();
    }
    let discovered = if cfg.models.discovery {
        discovered_models(registry)
    } else {
        Vec::new()
    };
    let pins = effective_pins(cfg);
    let mut out = Vec::new();
    for vendor in catalogue::vendors() {
        let (_, notes) = catalogue::resolved_ladder_avoiding(vendor, &discovered, &pins, &avoid);
        for note in notes {
            out.push(match (note.replacement, note.kept_because) {
                (Some(to), _) => format!("{} avoided; using {to}", note.avoided),
                (None, Some("pinned")) => format!(
                    "{} is avoided but an explicit pin wins; keeping it",
                    note.avoided
                ),
                (None, _) => format!(
                    "{} is avoided but is the last usable model of its tier; keeping it",
                    note.avoided
                ),
            });
        }
    }
    out
}

fn refresh_due(cfg: &CtxConfig, state: &StateDir, now: u64) -> bool {
    let registry_stale = cfg.models.discovery
        && now.saturating_sub(load_registry(state).updated_at) > REGISTRY_MAX_AGE_SECS;
    let prices_stale = cfg.models.price_fetch
        && now.saturating_sub(load_price_cache(state).fetched_at) > PRICE_MAX_AGE_SECS;
    let scorecard_stale = cfg.models.auto_avoid
        && now.saturating_sub(scorecard::load(state).map_or(0, |card| card.generated_at))
            > REGISTRY_MAX_AGE_SECS;
    if !registry_stale && !prices_stale && !scorecard_stale {
        return false;
    }
    let last_attempt = std::fs::read_to_string(state.root().join(REFRESH_ATTEMPT_FILE))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    now.saturating_sub(last_attempt) >= REFRESH_BACKOFF_SECS
}

/// Starts one background refresh when the registry is older than 24 h or the price cache
/// older than 7 days. Spawn-and-forget: `spawn` must not wait. Returns whether it spawned.
pub(crate) fn spawn_refresh_if_due(
    cfg: &CtxConfig,
    state: &StateDir,
    now: u64,
    spawn: &mut dyn FnMut() -> bool,
) -> bool {
    if !refresh_due(cfg, state, now) {
        return false;
    }
    if std::fs::create_dir_all(state.root()).is_err() {
        return false;
    }
    // A held lock means a refresher is running; the check lock is released at once.
    if state::try_acquire_lock(&state.root().join(REFRESH_LOCK_FILE)).is_err() {
        return false;
    }
    // Recorded before the spawn so a failed refresh backs off for an hour too.
    if state::write_private(&state.root().join(REFRESH_ATTEMPT_FILE), &now.to_string()).is_err() {
        return false;
    }
    spawn()
}

/// The detached production trigger for `ctx status` and dashboard startup. Never run from a
/// hook, never waits; a no-op in test builds.
pub(crate) fn spawn_refresh_if_due_detached(cfg: &CtxConfig, state: &StateDir) {
    #[cfg(not(test))]
    spawn_refresh_if_due(cfg, state, state::now_secs(), &mut || {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let mut command = std::process::Command::new(exe);
        command
            .args(["ctx", "models", "refresh", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::commands::workflow::engine::cli::detach(&mut command);
        command.spawn().is_ok()
    });
    #[cfg(test)]
    let _ = (cfg, state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_cache_fixture_is_parsed_with_retirement_and_visibility() {
        let rows = parse_codex_cache(
            include_str!("../../../../tests/fixtures/codex-models-cache.json"),
            10,
        );
        let retiring = rows
            .iter()
            .find(|row| row.id == "gpt-5.5")
            .expect("retiring model");
        assert!(retiring.available);
        assert_eq!(retiring.upgrade.as_deref(), Some("gpt-6.1-sol"));
        assert!(retiring.retirement_at.is_some());
        assert!(
            rows.iter()
                .any(|row| row.id == "codex-auto-review" && !row.available)
        );
    }

    #[test]
    fn a_large_transcript_cut_inside_a_multibyte_character_is_still_scanned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[]}}"#;
        // Multibyte filler so the cut (an odd offset from the end) splits a character.
        let filler = "\u{e5}".repeat(MAX_TRANSCRIPT_BYTES as usize / 2 + 1);
        let mut content = format!("{filler}\n{line}\n");
        // The filler's characters start at even offsets, so an odd cut splits one.
        if (content.len() as u64 - MAX_TRANSCRIPT_BYTES).is_multiple_of(2) {
            content = format!("{filler}\n{line} \n");
        }
        std::fs::write(&path, content).expect("write");
        let text = read_transcript_tail(&path).expect("tail");
        assert_eq!(text.trim(), line);
        assert_eq!(parse_claude_transcript(&text, 1).len(), 1);
    }

    #[test]
    fn transcript_observed_id_is_learned() {
        let rows = parse_claude_transcript(
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[]}}"#,
            20,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].family.as_deref(), Some("opus"));
        assert!(rows[0].available);
    }

    #[test]
    fn dedicated_family_pin_wins_over_other_explicit_model_config() {
        let mut cfg = CtxConfig::default();
        cfg.models
            .pin
            .insert("openai.sol".into(), "gpt-6-sol".into());
        cfg.worker.codex = Some("gpt-6.1-sol".into());
        assert_eq!(
            effective_pins(&cfg).get("openai.sol").map(String::as_str),
            Some("gpt-6-sol")
        );
    }

    #[test]
    fn corrupt_registry_is_a_cache_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(REGISTRY_FILE), "not json").expect("write");
        assert_eq!(
            load_registry(&StateDir::from_root(dir.path().to_path_buf())),
            Registry::default()
        );
    }

    #[test]
    fn remote_refresh_is_disabled_in_tests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        assert!(refresh_price_cache(&state, 10).is_err());
        assert!(!dir.path().join(PRICE_CACHE_FILE).exists());
    }

    #[test]
    fn epoch_dates_render_as_iso_dates() {
        assert_eq!(format_epoch_date(0), "1970-01-01");
        assert_eq!(format_epoch_date(1_799_798_400), "2027-01-13");
    }

    #[test]
    fn models_dev_and_litellm_fixtures_parse_per_million_prices() {
        let models_dev = parse_models_dev(
            r#"{"anthropic":{"models":{"claude-opus-5-5":{"cost":{"input":4,"output":20,"cache_read":0.2,"cache_write":5}}}}}"#,
        );
        let litellm = parse_litellm(
            r#"{"claude-opus-5-5":{"litellm_provider":"anthropic","input_cost_per_token":0.000004,"output_cost_per_token":0.00002,"cache_read_input_token_cost":0.0000002,"cache_creation_input_token_cost":0.000005},"anthropic/ignored":{"litellm_provider":"anthropic","input_cost_per_token":1,"output_cost_per_token":1}}"#,
        );
        assert_eq!(models_dev["claude-opus-5-5"], litellm["claude-opus-5-5"]);
        assert!(!litellm.contains_key("anthropic/ignored"));
    }

    #[test]
    fn price_disagreement_is_approximate() {
        let rate = ModelPrice {
            input_micros: 4_000_000,
            output_micros: 20_000_000,
            cache_read_micros: 400_000,
            cache_write_micros: 5_000_000,
        };
        let other = ModelPrice {
            input_micros: 2_000_000,
            output_micros: 10_000_000,
            cache_read_micros: 200_000,
            cache_write_micros: 2_500_000,
        };
        let cache = PriceCache {
            models_dev: BTreeMap::from([("gpt-5.6-sol".into(), rate)]),
            litellm: BTreeMap::from([("gpt-5.6-sol".into(), other)]),
            ..PriceCache::default()
        };
        assert!(effective_prices_from(&cache, None).provenance["gpt-5.6-sol"].approximate);
    }

    #[test]
    fn operator_override_wins_over_cache_then_snapshot() {
        let fetched = ModelPrice {
            input_micros: 1,
            output_micros: 2,
            cache_read_micros: 3,
            cache_write_micros: 4,
        };
        let operator = ModelPrice {
            input_micros: 9,
            output_micros: 8,
            cache_read_micros: 7,
            cache_write_micros: 6,
        };
        let cache = PriceCache {
            models_dev: BTreeMap::from([("claude-opus-5".into(), fetched)]),
            ..PriceCache::default()
        };
        let override_table = PriceTable {
            as_of: "2026-10-01".into(),
            models: BTreeMap::from([("claude-opus-5".into(), operator)]),
        };
        let effective = effective_prices_from(&cache, Some(override_table));
        assert_eq!(effective.table.models["claude-opus-5"], operator);
        assert!(effective.table.models.contains_key("claude-sonnet-5"));
    }

    #[test]
    fn offline_price_resolution_uses_cache_then_snapshot() {
        let fetched = ModelPrice {
            input_micros: 1,
            output_micros: 2,
            cache_read_micros: 3,
            cache_write_micros: 4,
        };
        let cache = PriceCache {
            models_dev: BTreeMap::from([("claude-opus-5".into(), fetched)]),
            ..PriceCache::default()
        };
        let effective = effective_prices_from(&cache, None);
        assert_eq!(effective.table.models["claude-opus-5"], fetched);
        assert!(effective.table.models.contains_key("claude-sonnet-5"));
    }

    #[test]
    fn configured_retiring_model_warns_with_upgrade_target() {
        let mut cfg = CtxConfig::default();
        cfg.worker.codex = Some("gpt-5.5".into());
        let model = RegistryModel {
            vendor: "openai".into(),
            id: "gpt-5.5".into(),
            available: true,
            retirement_at: Some(10 + 13 * 86_400),
            upgrade: Some("gpt-6.1-sol".into()),
            ..RegistryModel::default()
        };
        let registry = Registry {
            updated_at: 10,
            models: BTreeMap::from([("openai:gpt-5.5".into(), model)]),
        };
        assert_eq!(
            retirement_warnings(&cfg, &registry, 10),
            vec!["model gpt-5.5 retires within 14 days; upgrade to gpt-6.1-sol"]
        );
    }

    fn due_state() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        (tmp, state)
    }

    fn count_spawns(cfg: &CtxConfig, state: &StateDir, now: u64) -> u32 {
        let mut spawned = 0;
        spawn_refresh_if_due(cfg, state, now, &mut || {
            spawned += 1;
            true
        });
        spawned
    }

    #[test]
    fn a_stale_scorecard_alone_triggers_a_refresh_when_auto_avoid_is_on() {
        let (_tmp, state) = due_state();
        let mut cfg = CtxConfig::default();
        cfg.models.discovery = false;
        cfg.models.price_fetch = false;
        let now = 10 * 86_400;
        assert_eq!(count_spawns(&cfg, &state, now), 0, "auto_avoid off");
        cfg.models.auto_avoid = true;
        assert_eq!(count_spawns(&cfg, &state, now), 1, "no scorecard yet");
        write_json(
            &state.root().join(scorecard::SCORECARD_FILE),
            &scorecard::Scorecard {
                generated_at: now,
                ..Default::default()
            },
        )
        .expect("scorecard");
        assert_eq!(count_spawns(&cfg, &state, now + 3_600), 0, "fresh");
    }

    #[test]
    fn stale_data_spawns_exactly_one_refresher_then_backs_off() {
        let (_tmp, state) = due_state();
        let cfg = CtxConfig::default();
        let now = 10 * 86_400;
        assert_eq!(count_spawns(&cfg, &state, now), 1);
        // The attempt is recorded, so a failed refresh is not retried for an hour.
        assert_eq!(count_spawns(&cfg, &state, now + 3_599), 0);
        assert_eq!(count_spawns(&cfg, &state, now + 3_600), 1);
    }

    #[test]
    fn fresh_data_spawns_nothing() {
        let (_tmp, state) = due_state();
        let cfg = CtxConfig::default();
        let now = 10 * 86_400;
        write_json(
            &state.root().join(REGISTRY_FILE),
            &Registry {
                updated_at: now - 3_600,
                ..Registry::default()
            },
        )
        .expect("registry");
        write_json(
            &state.root().join(PRICE_CACHE_FILE),
            &PriceCache {
                fetched_at: now - 86_400,
                ..PriceCache::default()
            },
        )
        .expect("prices");
        assert_eq!(count_spawns(&cfg, &state, now), 0);
    }

    #[test]
    fn a_held_refresh_lock_spawns_nothing() {
        let (_tmp, state) = due_state();
        let cfg = CtxConfig::default();
        let _held = state::try_acquire_lock(&state.root().join(REFRESH_LOCK_FILE)).expect("lock");
        assert_eq!(count_spawns(&cfg, &state, 10 * 86_400), 0);
    }

    #[test]
    fn disabled_discovery_and_price_fetch_never_spawn() {
        let (_tmp, state) = due_state();
        let mut cfg = CtxConfig::default();
        cfg.models.discovery = false;
        cfg.models.price_fetch = false;
        assert_eq!(count_spawns(&cfg, &state, 10 * 86_400), 0);
    }

    #[test]
    fn runtime_ladder_is_empty_without_discovered_models() {
        set_test_discovered(Vec::new());
        let openai = catalogue::vendor("openai").expect("openai");
        assert!(runtime_ladder(openai).is_empty());
        assert!(ladder_for(&CtxConfig::default(), openai).is_empty());
    }
    fn observed_ids(vendor: &str, ids: &[&str]) {
        set_test_discovered(
            ids.iter()
                .map(|id| DiscoveredModel::new(vendor, *id, true))
                .collect(),
        );
    }

    fn cfg_avoiding(ids: &[&str]) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.models.avoid = ids.iter().map(|id| (*id).to_string()).collect();
        cfg
    }

    fn codex_deep(cfg: &CtxConfig) -> String {
        crate::commands::ctx::handover::resolve_model("codex", "deep", cfg).expect("deep")
    }

    #[test]
    fn avoid_substitutes_within_the_tier_and_never_goes_cheaper() {
        observed_ids("openai", &["gpt-6.1-sol"]);
        assert_eq!(codex_deep(&CtxConfig::default()), "gpt-6.1-sol");
        let cfg = cfg_avoiding(&["gpt-6.1-sol"]);
        // The same family's older version, never terra or luna.
        assert_eq!(codex_deep(&cfg), "gpt-5.6-sol");
        for tier in ["cheap", "standard"] {
            let plain = crate::commands::ctx::handover::resolve_model("codex", tier, &cfg);
            assert_eq!(
                plain.expect("tier"),
                crate::commands::ctx::handover::resolve_model("codex", tier, &CtxConfig::default())
                    .expect("tier"),
                "avoiding a deep model must not move {tier}"
            );
        }
        let openai = catalogue::vendor("openai").expect("openai");
        let (ladder, _) = catalogue::resolved_ladder_avoiding(
            openai,
            &[DiscoveredModel::new("openai", "gpt-5.5-sol", true)],
            &BTreeMap::new(),
            &avoid_ids(&["gpt-5.5-sol".to_string()]),
        );
        // A newer version of the family (the compiled one) is preferred over anything else.
        assert_eq!(ladder[1].id, "gpt-5.6-sol");
    }

    #[test]
    fn avoid_applies_to_the_static_ladder_whatever_the_discovery_state() {
        let openai = catalogue::vendor("openai").expect("openai");
        observed_ids("openai", &["gpt-6.1-sol"]);
        let mut cfg = cfg_avoiding(&["gpt-6.1-sol"]);
        cfg.models.discovery = false;
        let ladder = ladder_for(&cfg, openai);
        assert_eq!(ladder.len(), openai.rungs.len(), "avoid still resolves");
        assert!(ladder.iter().all(|r| r.id != "gpt-6.1-sol"), "{ladder:?}");
        cfg.models.avoid.clear();
        assert!(ladder_for(&cfg, openai).is_empty(), "no avoid: unchanged");

        // No registry model for the vendor: the static ladder is still avoided.
        set_test_discovered(Vec::new());
        let ladder = ladder_for(&cfg_avoiding(&["gpt-5.6-sol"]), openai);
        assert_eq!(ladder.len(), openai.rungs.len());
        assert!(ladder_for(&CtxConfig::default(), openai).is_empty());
    }

    #[test]
    fn config_less_callers_honour_the_operator_discovery_switch() {
        let openai = catalogue::vendor("openai").expect("openai");
        observed_ids("openai", &["gpt-6.1-sol"]);
        TEST_MODELS.with(|cell| cell.borrow_mut().discovery = false);
        let off = runtime_ladder(openai);
        TEST_MODELS.with(|cell| cell.borrow_mut().discovery = true);
        assert!(off.is_empty());
        assert!(!runtime_ladder(openai).is_empty());
    }

    #[test]
    fn config_less_callers_honour_the_environment_avoid_list() {
        observed_ids("openai", &["gpt-6.1-sol"]);
        let openai = catalogue::vendor("openai").expect("openai");
        assert_eq!(runtime_ladder(openai)[1].id, "gpt-6.1-sol");
        set_test_avoid(&["gpt-6.1-sol"]);
        let ladder = runtime_ladder(openai);
        assert_eq!(ladder[1].id, "gpt-5.6-sol");
        assert_eq!(ladder[2].id, "gpt-5.6-terra", "lower tiers are untouched");
        set_test_avoid(&[]);
    }

    #[test]
    fn a_toml_avoid_entry_changes_rung_below_like_the_environment_does() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[models]\navoid = [\"gpt-6.1-terra\"]\n",
        )
        .expect("write home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let openai = catalogue::vendor("openai").expect("openai");
        observed_ids("openai", &["gpt-6.1-terra"]);
        assert_eq!(
            catalogue::rung_below(openai, Some("gpt-5.6-sol")),
            "gpt-6.1-terra"
        );

        let loaded = ModelsConfig::load_operator_only(&|_| None).expect("load");
        TEST_MODELS.with(|cell| *cell.borrow_mut() = loaded);
        let from_toml = catalogue::rung_below(openai, Some("gpt-5.6-sol"));
        set_test_avoid(&["gpt-6.1-terra"]);
        let from_env_list = catalogue::rung_below(openai, Some("gpt-5.6-sol"));
        set_test_avoid(&[]);
        assert_eq!(from_toml, "gpt-5.6-terra");
        assert_eq!(from_toml, from_env_list);
    }

    #[test]
    fn avoid_keeps_the_last_model_of_a_tier_and_warns() {
        observed_ids("openai", &["gpt-6.1-sol"]);
        let cfg = cfg_avoiding(&["gpt-6.1-sol", "gpt-5.6-sol", "gpt-5.6-terra"]);
        assert_eq!(codex_deep(&cfg), "gpt-6.1-sol", "no unavoided sol is left");
        assert_eq!(
            crate::commands::ctx::handover::resolve_model("codex", "standard", &cfg)
                .expect("standard"),
            "gpt-5.6-terra",
            "terra is the only standard model"
        );
        let warnings = avoid_warnings(&cfg, &Registry::default());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("gpt-5.6-terra") && w.contains("last usable model of its tier")),
            "{warnings:?}"
        );
    }

    #[test]
    fn empty_avoid_and_auto_avoid_off_leave_every_resolver_unchanged() {
        use crate::commands::ctx::handover::{resolve_model, tier_for_model};
        // The refresher's set must be ignored while auto_avoid is off.
        set_test_cached_auto_avoid(&["gpt-6.1-sol", "claude-opus-5-5"]);
        for (vendor, ids) in [
            ("openai", vec![]),
            ("openai", vec!["gpt-6.1-sol"]),
            ("openai", vec!["gpt-6.1-sol", "gpt-6.2-astra"]),
            ("anthropic", vec!["claude-opus-5-5"]),
        ] {
            observed_ids(vendor, &ids);
            let cfg = CtxConfig::default();
            let slug = catalogue::vendor(vendor).expect("vendor");
            let discovered = discovered_for(vendor, &ids);
            let expected = if ids.is_empty() {
                Vec::new()
            } else {
                catalogue::resolved_ladder(slug, &discovered, &effective_pins(&cfg))
            };
            assert_eq!(ladder_for(&cfg, slug), expected, "{vendor} {ids:?}");
            assert_eq!(
                runtime_ladder(slug),
                if ids.is_empty() {
                    Vec::new()
                } else {
                    catalogue::resolved_ladder(slug, &discovered, &BTreeMap::new())
                }
            );
            for harness in ["claude", "codex"] {
                for tier in ["cheap", "standard", "deep"] {
                    let want = catalogue::vendor(if harness == "claude" {
                        "anthropic"
                    } else {
                        "openai"
                    })
                    .and_then(|v| {
                        let tier = match tier {
                            "cheap" => Tier::Cheap,
                            "standard" => Tier::Standard,
                            _ => Tier::Deep,
                        };
                        let plain = if v.slug == vendor && !ids.is_empty() {
                            catalogue::resolved_ladder(v, &discovered, &effective_pins(&cfg))
                        } else {
                            Vec::new()
                        };
                        catalogue::tier_model_in(v, tier, &plain)
                    });
                    assert_eq!(
                        resolve_model(harness, tier, &cfg).ok(),
                        want,
                        "{harness} {tier} with {ids:?}"
                    );
                }
            }
            assert_eq!(
                crate::commands::ctx::proxy::decision::worker_model(&cfg, "codex", Tier::Deep),
                resolve_model("codex", "deep", &cfg).expect("deep")
            );
            for model in ["gpt-6.1-sol", "claude-opus-5-5", "gpt-5.6-terra", "opus"] {
                let harness = if model.starts_with("gpt") {
                    "codex"
                } else {
                    "claude"
                };
                let _ = tier_for_model(harness, model, &cfg);
            }
        }
        set_test_cached_auto_avoid(&[]);
    }

    fn discovered_for(vendor: &str, ids: &[&str]) -> Vec<DiscoveredModel> {
        ids.iter()
            .map(|id| DiscoveredModel::new(vendor, *id, true))
            .collect()
    }

    #[test]
    fn an_explicit_pin_beats_avoid() {
        observed_ids("openai", &["gpt-6.1-sol"]);
        let mut cfg = cfg_avoiding(&["gpt-5.6-sol", "gpt-6.1-sol"]);
        cfg.worker.codex = Some("gpt-5.6-sol".into());
        assert_eq!(codex_deep(&cfg), "gpt-5.6-sol");
        let mut cfg = cfg_avoiding(&["gpt-6.1-sol"]);
        cfg.models
            .pin
            .insert("openai.sol".into(), "gpt-6.1-sol".into());
        assert_eq!(codex_deep(&cfg), "gpt-6.1-sol");
        let warnings = avoid_warnings(&cfg, &Registry::default());
        assert!(
            warnings.iter().any(|w| w.contains("explicit pin wins")),
            "{warnings:?}"
        );
    }

    #[test]
    fn claude_keeps_its_alias_when_the_observed_id_is_avoided() {
        observed_ids("anthropic", &["claude-opus-5-5"]);
        let cfg = cfg_avoiding(&["claude-opus-5-5"]);
        assert_eq!(
            crate::commands::ctx::handover::resolve_model("claude", "deep", &cfg).expect("deep"),
            "opus"
        );
        let registry = Registry {
            models: BTreeMap::from([(
                "anthropic:claude-opus-5-5".to_string(),
                RegistryModel {
                    vendor: "anthropic".into(),
                    id: "claude-opus-5-5".into(),
                    available: true,
                    ..RegistryModel::default()
                },
            )]),
            ..Registry::default()
        };
        let warnings = avoid_warnings(&cfg, &registry);
        assert!(
            warnings.iter().any(|w| w.contains("last usable model")),
            "{warnings:?}"
        );
    }

    #[test]
    fn auto_avoid_acts_only_when_enabled() {
        observed_ids("openai", &["gpt-6.1-sol"]);
        set_test_cached_auto_avoid(&["gpt-6.1-sol"]);
        let mut cfg = CtxConfig::default();
        assert_eq!(
            codex_deep(&cfg),
            "gpt-6.1-sol",
            "auto_avoid is off by default"
        );
        cfg.models.auto_avoid = true;
        assert_eq!(codex_deep(&cfg), "gpt-5.6-sol");
        set_test_cached_auto_avoid(&[]);
    }

    #[test]
    fn the_refresher_caches_the_scorecard_and_logs_new_auto_avoid_decisions() {
        let (_tmp, state) = due_state();
        let table = price::built_in_table();
        let log = state.logs();
        std::fs::create_dir_all(&log).expect("logs");
        let mut lines = String::new();
        let row = |model: &str, ok: bool| {
            format!(
                r#"{{"ts":1,"session":"s","parent_session":"p","agent":"codex","model":"{model}","input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":9,"wall_ms":1000,"exit_code":{},"outcome":"{}"}}"#,
                i32::from(!ok),
                if ok { "ok" } else { "failed" }
            )
        };
        for _ in 0..40 {
            lines.push_str(&row("sonnet", true));
            lines.push('\n');
        }
        for i in 0..40 {
            lines.push_str(&row("gpt-5.6-terra", i < 2));
            lines.push('\n');
        }
        std::fs::write(log.join("delegations.jsonl"), lines).expect("delegations");
        scorecard::refresh(&state, &table, 5).expect("refresh");
        scorecard::refresh(&state, &table, 6).expect("second refresh");
        let card = scorecard::load(&state).expect("cached scorecard");
        assert_eq!(
            card.auto_avoid_ids(),
            BTreeSet::from(["gpt-5.6-terra".to_string()])
        );
        let logged = std::fs::read_to_string(log.join("model-auto-avoid.jsonl")).expect("log");
        assert_eq!(
            logged.lines().count(),
            1,
            "a decision is logged once: {logged}"
        );
        assert!(logged.contains("sonnet") && logged.contains("peer_low"));

        let mut cfg = CtxConfig::default();
        assert!(avoid_for_state(&cfg, &state).is_empty());
        cfg.models.auto_avoid = true;
        cfg.models.avoid = vec!["gpt-5.6-sol".into()];
        assert_eq!(
            avoid_for_state(&cfg, &state),
            BTreeSet::from(["gpt-5.6-sol".to_string(), "gpt-5.6-terra".to_string()])
        );
    }
}
