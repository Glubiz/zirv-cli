//! Pure, deterministic secret and personal-data masking.
//!
//! Detection and replacement live here; filesystem locking and persistence
//! belong to `obfuscate_store`.  A vault stores canonical placeholder stems,
//! so changing the email-domain policy never changes an existing identity.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueClass {
    Secret,
    Pii,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub value: String,
    /// Canonical placeholder without an email-domain shape hint.
    pub placeholder: String,
    pub kind: String,
    pub class: ValueClass,
    pub first_seen_surface: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vault {
    entries: Vec<VaultEntry>,
    /// Encoded forms of secret values, computed once per load/insert.
    variants: Vec<String>,
}

/// Replaces encoded forms of vault secrets. It is not a `ZIRV_SECRET_`/`ZIRV_PII_`
/// placeholder, so `rehydrate` never turns it back into a value.
pub const ENCODED_MARKER: &str = "ZIRV_REDACTED_ENCODED";

const MIN_VARIANT_LEN: usize = 8;

fn base64_nopad(bytes: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(alphabet[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// Base64 (standard and URL-safe, unpadded), percent, lower/upper hex and
/// reversed forms of `value` that differ from it. Empty below the length floor.
fn encoded_variants(value: &str) -> Vec<String> {
    if value.chars().count() < MIN_VARIANT_LEN {
        return Vec::new();
    }
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = value.as_bytes();
    let mut percent = String::with_capacity(value.len() * 3);
    let mut hex = String::with_capacity(value.len() * 2);
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            percent.push(char::from(*byte));
        } else {
            percent.push_str(&format!("%{byte:02X}"));
        }
        hex.push_str(&format!("{byte:02x}"));
    }
    let mut variants = vec![
        base64_nopad(bytes, STD),
        base64_nopad(bytes, URL),
        percent,
        hex.to_ascii_uppercase(),
        hex,
        value.chars().rev().collect(),
    ];
    variants.retain(|variant| variant != value);
    variants.sort();
    variants.dedup();
    variants
}

impl Vault {
    pub fn from_entries(entries: Vec<VaultEntry>) -> Result<Self, String> {
        let mut values = HashSet::new();
        let mut placeholders = HashSet::new();
        for entry in &entries {
            if entry.value.is_empty()
                || entry.kind.is_empty()
                || !valid_placeholder(&entry.placeholder, entry.class, &entry.kind)
            {
                return Err("invalid obfuscation vault row".to_string());
            }
            if !values.insert(entry.value.clone())
                || !placeholders.insert(entry.placeholder.clone())
            {
                return Err("duplicate value or placeholder in obfuscation vault".to_string());
            }
        }
        let variants = entries
            .iter()
            .filter(|entry| entry.class == ValueClass::Secret)
            .flat_map(|entry| encoded_variants(&entry.value))
            .collect();
        Ok(Self { entries, variants })
    }

    pub fn entries(&self) -> &[VaultEntry] {
        &self.entries
    }

    fn entry_for_value(&self, value: &str) -> Option<&VaultEntry> {
        self.entries.iter().find(|entry| entry.value == value)
    }

    fn insert(&mut self, value: &str, kind: &str, class: ValueClass, surface: &str) -> String {
        if let Some(entry) = self.entry_for_value(value) {
            return entry.placeholder.clone();
        }
        let prefix = match class {
            ValueClass::Secret => "ZIRV_SECRET",
            ValueClass::Pii => "ZIRV_PII",
        };
        let kind = normalize_kind(kind);
        let next = self
            .entries
            .iter()
            .filter(|entry| entry.class == class && entry.kind == kind)
            .count()
            + 1;
        let placeholder = format!("{prefix}_{kind}_{next}");
        if class == ValueClass::Secret {
            self.variants.extend(encoded_variants(value));
        }
        self.entries.push(VaultEntry {
            value: value.to_string(),
            placeholder: placeholder.clone(),
            kind,
            class,
            first_seen_surface: surface.to_string(),
        });
        placeholder
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    Off,
    Flag,
    #[default]
    Obfuscate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntropyMode {
    #[default]
    Flag,
    Obfuscate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmailDomain {
    #[default]
    Keep,
    Mask,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorPattern {
    pub kind: String,
    pub regex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub mode: Mode,
    pub entropy: EntropyMode,
    pub email_domain: EmailDomain,
    pub patterns: Vec<OperatorPattern>,
    pub literals: Vec<String>,
    /// Literal or regex entries. A valid regex is used as a regex; an invalid
    /// one is treated literally so a typo never broadens what leaves device.
    pub allow: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Obfuscate,
            entropy: EntropyMode::Flag,
            email_domain: EmailDomain::Keep,
            patterns: Vec::new(),
            literals: Vec::new(),
            allow: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: String,
    pub class: ValueClass,
    pub placeholder: String,
    pub replaced: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    start: usize,
    end: usize,
    kind: String,
    class: ValueClass,
    entropy_only: bool,
    /// Encoded form of a vault secret: masked with `ENCODED_MARKER`, never stored.
    encoded: bool,
}

impl Candidate {
    fn replaces(&self, options: &Options) -> bool {
        options.mode == Mode::Obfuscate
            && (!self.entropy_only || options.entropy == EntropyMode::Obfuscate)
    }
}

/// Replaces supported values with stable, typed placeholders. No I/O, clock,
/// environment, or network access occurs here.
pub fn obfuscate(
    text: &str,
    vault: &mut Vault,
    options: &Options,
    surface: &str,
) -> (String, Vec<Finding>) {
    if options.mode == Mode::Off || text.is_empty() {
        return (text.to_string(), Vec::new());
    }

    let allowed = allowed_ranges(text, &options.allow);
    let protected: Vec<_> = placeholders(text)
        .map(|(full, _)| (full.start(), full.end()))
        .collect();
    let mut candidates = candidates(text, options);
    candidates.extend(encoded_candidates(text, vault, options));
    candidates.retain(|candidate| {
        !allowed
            .iter()
            .any(|(start, end)| candidate.start < *end && candidate.end > *start)
            && !protected
                .iter()
                .any(|(start, end)| candidate.start >= *start && candidate.end <= *end)
    });
    // Issue #466: flag-only spans must never suppress a replacing detector.
    candidates.sort_by(|a, b| {
        b.replaces(options)
            .cmp(&a.replaces(options))
            .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
            .then_with(|| a.start.cmp(&b.start))
    });

    let mut selected: BTreeMap<usize, Candidate> = BTreeMap::new();
    for candidate in candidates {
        if selected
            .range(..candidate.end)
            .next_back()
            .is_none_or(|(_, prior)| prior.end <= candidate.start)
        {
            selected.entry(candidate.start).or_insert(candidate);
        }
    }

    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut findings = Vec::with_capacity(selected.len());
    for candidate in selected.into_values() {
        let value = &text[candidate.start..candidate.end];
        if candidate.encoded {
            let replace = candidate.replaces(options);
            output.push_str(&text[cursor..candidate.start]);
            output.push_str(if replace { ENCODED_MARKER } else { value });
            cursor = candidate.end;
            findings.push(Finding {
                kind: candidate.kind,
                class: candidate.class,
                placeholder: ENCODED_MARKER.to_string(),
                replaced: replace,
            });
            continue;
        }
        let stem = vault.insert(value, &candidate.kind, candidate.class, surface);
        let replacement = if candidate.kind == "EMAIL" && options.email_domain == EmailDomain::Keep
        {
            value
                .rsplit_once('@')
                .map(|(_, domain)| format!("{stem}@{domain}"))
                .unwrap_or_else(|| stem.clone())
        } else {
            stem.clone()
        };
        let replace = candidate.replaces(options);
        output.push_str(&text[cursor..candidate.start]);
        output.push_str(if replace { &replacement } else { value });
        cursor = candidate.end;
        findings.push(Finding {
            kind: candidate.kind,
            class: candidate.class,
            placeholder: replacement,
            replaced: replace,
        });
    }
    output.push_str(&text[cursor..]);
    (output, findings)
}

/// Restores whole canonical and email-shaped placeholders in a single pass.
pub fn rehydrate(text: &str, vault: &Vault) -> String {
    let entries: HashMap<_, _> = vault
        .entries
        .iter()
        .map(|entry| (entry.placeholder.as_str(), entry))
        .collect();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for (full, stem) in placeholders(text) {
        output.push_str(&text[cursor..full.start()]);
        if let Some(entry) = entries.get(stem.as_str()) {
            output.push_str(&entry.value);
            let suffix = &text[stem.end()..full.end()];
            let domain = entry.value.rsplit_once('@').map(|(_, domain)| domain);
            if entry.kind != "EMAIL" || suffix.strip_prefix('@') != domain {
                output.push_str(suffix);
            }
        } else {
            output.push_str(full.as_str());
        }
        cursor = full.end();
    }
    output.push_str(&text[cursor..]);
    output
}

fn placeholders(text: &str) -> impl Iterator<Item = (regex::Match<'_>, regex::Match<'_>)> {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(ZIRV_(?:SECRET|PII)_[A-Z0-9](?:[A-Z0-9_]*[A-Z0-9])?_([0-9]+))\b(?:@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z]{2,})+\b)?",
        )
        .expect("valid Zirv placeholder regex")
    });
    PATTERN.captures_iter(text).filter_map(|captures| {
        if captures.get(2)?.as_str() == "0" {
            return None;
        }
        Some((captures.get(0)?, captures.get(1)?))
    })
}

pub fn contains_placeholder(text: &str) -> bool {
    text.contains("ZIRV_SECRET_") || text.contains("ZIRV_PII_")
}

fn encoded_candidates(text: &str, vault: &Vault, options: &Options) -> Vec<Candidate> {
    let literal_variants: Vec<String> = options
        .literals
        .iter()
        .flat_map(|literal| encoded_variants(literal))
        .collect();
    vault
        .variants
        .iter()
        .chain(&literal_variants)
        .flat_map(|variant| text.match_indices(variant.as_str()))
        .map(|(start, matched)| Candidate {
            start,
            end: start + matched.len(),
            kind: "ENCODED_SECRET".to_string(),
            class: ValueClass::Secret,
            entropy_only: false,
            encoded: true,
        })
        .collect()
}

fn candidates(text: &str, options: &Options) -> Vec<Candidate> {
    let mut found = Vec::new();
    let builtins = [
        (
            ValueClass::Secret,
            "PEM_PRIVATE_KEY",
            r"(?s)-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----.*?-----END (?:RSA |EC |OPENSSH )?PRIVATE KEY-----",
        ),
        (
            ValueClass::Secret,
            "OPENAI_KEY",
            r"\bsk-[A-Za-z0-9_-]{16,}\b",
        ),
        (
            ValueClass::Secret,
            "STRIPE_KEY",
            r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}\b",
        ),
        (
            ValueClass::Secret,
            "GITHUB_TOKEN",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b",
        ),
        (
            ValueClass::Secret,
            "SLACK_TOKEN",
            r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
        ),
        (
            ValueClass::Secret,
            "GOOGLE_API_KEY",
            r"\bAIza[A-Za-z0-9_-]{30,}\b",
        ),
        (ValueClass::Secret, "NPM_TOKEN", r"\bnpm_[A-Za-z0-9]{20,}\b"),
        (
            ValueClass::Secret,
            "AWS_ACCESS_KEY_ID",
            r"\bA[SK]IA[0-9A-Z]{16}\b",
        ),
        (
            ValueClass::Secret,
            "JWT",
            r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",
        ),
        (
            ValueClass::Secret,
            "URL_CREDENTIALS",
            r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^\s/:@]+:[^\s/@]+@[^\s]+",
        ),
        (
            ValueClass::Pii,
            "EMAIL",
            r"\b[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z]{2,})+\b",
        ),
    ];
    for (class, kind, pattern) in builtins {
        if let Ok(regex) = Regex::new(pattern) {
            add_matches(&mut found, text, &regex, kind, class, false);
        }
    }

    // Validator-backed numeric PII uses a deliberately broad candidate regex.
    if let Ok(regex) = Regex::new(r"\b\d{6}-?\d{4}\b") {
        for matched in regex.find_iter(text) {
            if valid_cpr(matched.as_str()) {
                found.push(candidate(
                    matched.start(),
                    matched.end(),
                    "CPR",
                    ValueClass::Pii,
                    false,
                ));
            }
        }
    }
    if let Ok(regex) = Regex::new(r"\b[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]){11,30}\b") {
        for matched in regex.find_iter(text) {
            if valid_iban(matched.as_str()) {
                found.push(candidate(
                    matched.start(),
                    matched.end(),
                    "IBAN",
                    ValueClass::Pii,
                    false,
                ));
            }
        }
    }
    if let Ok(regex) = Regex::new(r"\b\d(?:[ -]?\d){11,17}\d\b") {
        for matched in regex.find_iter(text) {
            if valid_card(matched.as_str()) {
                found.push(candidate(
                    matched.start(),
                    matched.end(),
                    "PAYMENT_CARD",
                    ValueClass::Pii,
                    false,
                ));
            }
        }
    }
    if let Ok(regex) = Regex::new(r"(?:\+|00)\d(?:[ ()-]?\d){7,14}|\b\d{2,4}(?:[ -]\d{2,4}){2,5}\b")
    {
        for matched in regex.find_iter(text) {
            let digits = matched
                .as_str()
                .chars()
                .filter(char::is_ascii_digit)
                .count();
            if (8..=15).contains(&digits) {
                found.push(candidate(
                    matched.start(),
                    matched.end(),
                    "PHONE",
                    ValueClass::Pii,
                    false,
                ));
            }
        }
    }

    for pattern in &options.patterns {
        if let Ok(regex) = Regex::new(&pattern.regex) {
            add_matches(
                &mut found,
                text,
                &regex,
                &pattern.kind,
                ValueClass::Secret,
                false,
            );
        }
    }
    for literal in &options.literals {
        if literal.is_empty() {
            continue;
        }
        for (start, _) in text.match_indices(literal) {
            found.push(candidate(
                start,
                start + literal.len(),
                "LITERAL",
                ValueClass::Secret,
                false,
            ));
        }
    }

    if let Ok(regex) = Regex::new(r"\b[A-Za-z0-9+/=_-]{32,}\b") {
        for matched in regex.find_iter(text) {
            let value = matched.as_str();
            if high_entropy(value) && !looks_like_sha_uuid_or_path(value) {
                found.push(candidate(
                    matched.start(),
                    matched.end(),
                    "HIGH_ENTROPY",
                    ValueClass::Secret,
                    true,
                ));
            }
        }
    }
    found
}

fn add_matches(
    out: &mut Vec<Candidate>,
    text: &str,
    regex: &Regex,
    kind: &str,
    class: ValueClass,
    entropy_only: bool,
) {
    for matched in regex.find_iter(text) {
        out.push(candidate(
            matched.start(),
            matched.end(),
            kind,
            class,
            entropy_only,
        ));
    }
}

fn candidate(
    start: usize,
    end: usize,
    kind: &str,
    class: ValueClass,
    entropy_only: bool,
) -> Candidate {
    Candidate {
        start,
        end,
        kind: normalize_kind(kind),
        class,
        entropy_only,
        encoded: false,
    }
}

fn allowed_ranges(text: &str, allow: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for entry in allow {
        if entry.is_empty() {
            continue;
        }
        match Regex::new(entry) {
            Ok(regex) => ranges.extend(regex.find_iter(text).map(|m| (m.start(), m.end()))),
            Err(_) => ranges.extend(
                text.match_indices(entry)
                    .map(|(start, value)| (start, start + value.len())),
            ),
        }
    }
    ranges
}

fn valid_cpr(value: &str) -> bool {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 10 {
        return false;
    }
    let day = digits[0..2].parse::<u32>().unwrap_or(0);
    let month = digits[2..4].parse::<u32>().unwrap_or(0);
    let year = digits[4..6].parse::<u32>().unwrap_or(0);
    day >= 1 && day <= days_in_month(month, year)
}

fn days_in_month(month: u32, year: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) => 29,
        2 => 28,
        _ => 0,
    }
}

fn valid_iban(value: &str) -> bool {
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    if !(15..=34).contains(&compact.len()) || !compact.is_ascii() {
        return false;
    }
    let known_lengths: HashMap<&str, usize> = [
        ("DK", 18),
        ("DE", 22),
        ("GB", 22),
        ("NO", 15),
        ("SE", 24),
        ("FI", 18),
        ("FR", 27),
        ("ES", 24),
        ("IT", 27),
        ("NL", 18),
        ("BE", 16),
        ("CH", 21),
    ]
    .into_iter()
    .collect();
    if let Some(expected) = known_lengths.get(&compact[0..2])
        && compact.len() != *expected
    {
        return false;
    }
    let rearranged = format!("{}{}", &compact[4..], &compact[..4]);
    let mut remainder = 0u32;
    for c in rearranged.chars() {
        let encoded = if c.is_ascii_digit() {
            c.to_string()
        } else if c.is_ascii_uppercase() {
            ((c as u8 - b'A') + 10).to_string()
        } else {
            return false;
        };
        for digit in encoded.bytes() {
            remainder = (remainder * 10 + u32::from(digit - b'0')) % 97;
        }
    }
    remainder == 1
}

fn valid_card(value: &str) -> bool {
    let digits: Vec<u32> = value.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) || digits.iter().all(|digit| *digit == digits[0]) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            if index % 2 == 1 {
                let doubled = digit * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                *digit
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn high_entropy(value: &str) -> bool {
    let mut counts = HashMap::new();
    for byte in value.bytes() {
        *counts.entry(byte).or_insert(0usize) += 1;
    }
    let len = value.len() as f64;
    let entropy = counts.values().fold(0.0, |sum, count| {
        let p = *count as f64 / len;
        sum - p * p.log2()
    });
    entropy >= 4.2
}

fn looks_like_sha_uuid_or_path(value: &str) -> bool {
    (value.bytes().all(|b| b.is_ascii_hexdigit()) && matches!(value.len(), 40 | 64))
        || value.contains('/')
        || value.contains('\\')
}

fn normalize_kind(kind: &str) -> String {
    let normalized: String = kind
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    normalized.trim_matches('_').to_string()
}

fn valid_placeholder(placeholder: &str, class: ValueClass, kind: &str) -> bool {
    let prefix = match class {
        ValueClass::Secret => "ZIRV_SECRET_",
        ValueClass::Pii => "ZIRV_PII_",
    };
    let Some(rest) = placeholder.strip_prefix(prefix) else {
        return false;
    };
    let Some((actual_kind, number)) = rest.rsplit_once('_') else {
        return false;
    };
    actual_kind == normalize_kind(kind)
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number != "0"
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCODED_SECRET: &str = "pa$$w0rd/+?ok";

    fn seeded_vault(value: &str, class: ValueClass) -> Vault {
        Vault::from_entries(vec![VaultEntry {
            value: value.to_string(),
            placeholder: match class {
                ValueClass::Secret => "ZIRV_SECRET_LITERAL_1",
                ValueClass::Pii => "ZIRV_PII_LITERAL_1",
            }
            .to_string(),
            kind: "LITERAL".to_string(),
            class,
            first_seen_surface: "test".to_string(),
        }])
        .expect("valid vault")
    }

    #[test]
    fn encoded_variants_of_vault_secrets_are_masked_with_a_non_rehydrating_marker() {
        for variant in [
            "cGEkJHcwcmQvKz9vaw",
            "70612424773072642f2b3f6f6b",
            "70612424773072642F2B3F6F6B",
            "pa%24%24w0rd%2F%2B%3Fok",
            "ko?+/dr0w$$ap",
        ] {
            let mut vault = seeded_vault(ENCODED_SECRET, ValueClass::Secret);
            let before = vault.clone();
            let source = format!("out: {variant} end");
            let (masked, findings) = obfuscate(&source, &mut vault, &Options::default(), "test");
            assert_eq!(masked, format!("out: {ENCODED_MARKER} end"), "{variant}");
            assert_eq!(findings.len(), 1);
            assert!(findings[0].replaced);
            assert_eq!(vault, before);
            assert_eq!(rehydrate(&masked, &vault), masked);
            assert!(!contains_placeholder(&masked));
        }
    }

    #[test]
    fn url_safe_base64_variant_is_masked() {
        let secret = "ab?>cd~~xyz!!";
        let mut vault = seeded_vault(secret, ValueClass::Secret);
        let (masked, _) = obfuscate("YWI_PmNkfn54eXohIQ", &mut vault, &Options::default(), "t");
        assert_eq!(masked, ENCODED_MARKER);
    }

    #[test]
    fn short_and_pii_vault_values_get_no_variants() {
        let mut vault = seeded_vault("short12", ValueClass::Secret);
        let (masked, _) = obfuscate("c2hvcnQxMg 21trohs", &mut vault, &Options::default(), "t");
        assert!(!masked.contains(ENCODED_MARKER));
        let mut vault = seeded_vault(ENCODED_SECRET, ValueClass::Pii);
        let (masked, _) = obfuscate("cGEkJHcwcmQvKz9vaw", &mut vault, &Options::default(), "t");
        assert!(!masked.contains(ENCODED_MARKER));
    }

    #[test]
    fn ordinary_text_is_unchanged_by_variant_matching() {
        let mut vault = seeded_vault(ENCODED_SECRET, ValueClass::Secret);
        let source = "cargo build finished in 2.3s with 0 warnings";
        let (masked, findings) = obfuscate(source, &mut vault, &Options::default(), "t");
        assert_eq!(masked, source);
        assert!(findings.is_empty());
    }

    #[test]
    fn encoded_variants_of_operator_literals_are_masked() {
        let options = Options {
            literals: vec![ENCODED_SECRET.to_string()],
            ..Options::default()
        };
        let mut vault = Vault::default();
        let (masked, _) = obfuscate("x cGEkJHcwcmQvKz9vaw y", &mut vault, &options, "t");
        assert_eq!(masked, format!("x {ENCODED_MARKER} y"));
    }

    #[test]
    fn deterministic_identity_and_round_trip() {
        let options = Options::default();
        let mut vault = Vault::default();
        let source = "mail jane@company.dk key ghp_abcdefghijklmnopqrstuvwxyz123456";
        let (first, findings) = obfuscate(source, &mut vault, &options, "test");
        let (second, _) = obfuscate(source, &mut vault, &options, "other");
        assert_eq!(first, second);
        assert_eq!(findings.len(), 2);
        assert!(first.contains("ZIRV_PII_EMAIL_1@company.dk"));
        assert!(first.contains("ZIRV_SECRET_GITHUB_TOKEN_1"));
        assert_eq!(rehydrate(&first, &vault), source);
    }

    #[test]
    fn credential_masking_takes_precedence_over_overlapping_entropy_flags() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz123456";
        for (source, expected) in [
            (format!("TOKEN={token}"), "TOKEN=ZIRV_SECRET_GITHUB_TOKEN_1"),
            (format!("{token}=tail"), "ZIRV_SECRET_GITHUB_TOKEN_1=tail"),
        ] {
            let mut vault = Vault::default();
            let (masked, findings) = obfuscate(&source, &mut vault, &Options::default(), "test");
            assert_eq!(masked, expected);
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].kind, "GITHUB_TOKEN");
            assert!(findings[0].replaced);
            assert_eq!(rehydrate(&masked, &vault), source);
        }
    }

    #[test]
    fn entropy_without_a_credential_match_is_still_flagged() {
        let source = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
        let mut vault = Vault::default();
        let (flagged, findings) = obfuscate(source, &mut vault, &Options::default(), "test");
        assert_eq!(flagged, source);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, "HIGH_ENTROPY");
        assert!(!findings[0].replaced);
    }

    #[test]
    fn protecting_placeholders_is_idempotent_for_every_kind() {
        let mut vault = Vault::default();
        let options = Options::default();
        let source = "jane@company.dk";
        let (first, _) = obfuscate(source, &mut vault, &options, "mail");
        assert_eq!(first, "ZIRV_PII_EMAIL_1@company.dk");
        let original_vault = vault.clone();
        let (second, findings) = obfuscate(&first, &mut vault, &options, "prompt");
        assert_eq!(second, first);
        assert!(findings.is_empty());
        assert_eq!(vault, original_vault);
        assert_eq!(rehydrate(&second, &vault), source);

        let options = Options {
            entropy: EntropyMode::Obfuscate,
            patterns: vec![
                OperatorPattern {
                    kind: "CUSTOM__KIND".into(),
                    regex: "private-custom-value".into(),
                },
                OperatorPattern {
                    kind: "PLACEHOLDER_MATCH".into(),
                    regex: r"\bZIRV_(?:SECRET|PII)_[A-Z0-9_]+\b".into(),
                },
            ],
            literals: vec!["privatevalue".into(), "ZIRV_SECRET_LITERAL_1".into()],
            ..Options::default()
        };
        for source in [
            "jane@company.dk",
            "ghp_abcdefghijklmnopqrstuvwxyz123456",
            "010190-1234",
            "DK5000400440116243",
            "4242 4242 4242 4242",
            "+45 12 34 56 78",
            "privatevalue",
            "private-custom-value",
            "abcdefghijklmnopqrstuvwxyz0123456789ABCD",
            "https://ZIRV_PII_EMAIL_99:password@company.dk",
        ] {
            let mut vault = Vault::default();
            let (first, _) = obfuscate(source, &mut vault, &options, "mail");
            assert_ne!(first, source);
            let original_vault = vault.clone();
            let (second, findings) = obfuscate(&first, &mut vault, &options, "prompt");
            assert_eq!(second, first);
            assert!(findings.is_empty());
            assert_eq!(vault, original_vault);
            assert_eq!(rehydrate(&second, &vault), source);
        }
        for placeholder in ["ZIRV_SECRET_FUTURE_KIND_99", "ZIRV_PII_EMAIL_99@company.dk"] {
            let mut vault = Vault::default();
            let (masked, findings) = obfuscate(placeholder, &mut vault, &options, "prompt");
            assert_eq!(masked, placeholder);
            assert!(findings.is_empty());
            assert!(vault.entries().is_empty());
        }
    }

    #[test]
    fn unknown_placeholder_index_stays_unresolved_and_blocks_device_action() {
        let mut vault = Vault::default();
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";
        let (known, _) = obfuscate(secret, &mut vault, &Options::default(), "test");
        assert_eq!(known, "ZIRV_SECRET_GITHUB_TOKEN_1");
        for unresolved in [
            "ZIRV_SECRET_GITHUB_TOKEN_10",
            "ZIRV_SECRET_GITHUB_TOKEN_1suffix",
            "ZIRV_SECRET_GITHUB_TOKEN_1_0",
            "prefixZIRV_SECRET_GITHUB_TOKEN_1",
        ] {
            let restored = rehydrate(unresolved, &vault);
            assert_eq!(restored, unresolved);
            assert!(!restored.contains(secret));
            assert!(contains_placeholder(&restored));
        }
        assert_eq!(
            rehydrate(&format!("({known}),{known}"), &vault),
            format!("({secret}),{secret}")
        );

        let state_dir = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let state = super::super::state::StateDir::from_root(state_dir.path().to_path_buf());
        super::super::obfuscate_store::with_vault(
            &super::super::obfuscate_store::vault_path(state.root(), repo.path()),
            |stored| {
                *stored = vault.clone();
                Ok(())
            },
        )
        .expect("seed vault");
        let unknown = "ZIRV_SECRET_GITHUB_TOKEN_10";
        let mut input = serde_json::json!({"command": format!("printf %s {unknown}")});
        let error =
            super::super::obfuscate_store::rehydrate_json(state.root(), repo.path(), &mut input)
                .expect_err("unknown placeholder must refuse the device action");
        assert_eq!(
            error.to_string(),
            "unknown Zirv placeholder; refusing device action"
        );
        assert!(!input.to_string().contains(secret));
        let state_root = state.root().display().to_string();
        let env = |key: &str| match key {
            super::super::state::STATE_ENV => Some(state_root.clone()),
            "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".to_string()),
            _ => None,
        };
        let stdin = serde_json::json!({
            "session_id": "s1", "cwd": repo.path(), "tool_name": "Bash",
            "tool_input": input,
        })
        .to_string();
        let mut out = Vec::new();
        super::super::hook::run_pretool_for_agent(&mut out, &stdin, &env, None).expect("hook");
        let output: serde_json::Value = serde_json::from_slice(&out).expect("refusal");
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(!output.to_string().contains(secret));
        assert!(
            super::super::log::read_decisions(&state)
                .iter()
                .any(|decision| {
                    decision.action == "obfuscate-rehydration-miss" && decision.verdict == "blocked"
                })
        );
    }

    #[test]
    fn email_mask_uses_canonical_existing_vault_placeholder() {
        let mut vault = Vault::default();
        let keep = Options::default();
        let (kept, _) = obfuscate("jane@company.dk", &mut vault, &keep, "prompt");
        let mut mask = keep;
        mask.email_domain = EmailDomain::Mask;
        let (masked, _) = obfuscate("jane@company.dk", &mut vault, &mask, "prompt");
        assert_eq!(kept, "ZIRV_PII_EMAIL_1@company.dk");
        assert_eq!(masked, "ZIRV_PII_EMAIL_1");
        assert_eq!(rehydrate(&masked, &vault), "jane@company.dk");
    }

    #[test]
    fn validates_numeric_pii_and_full_pem_blocks() {
        let mut vault = Vault::default();
        let source = "CPR 010190-1234 IBAN DK5000400440116243 card 4242 4242 4242 4242\n-----BEGIN PRIVATE KEY-----\nabcDEF123+/=\n-----END PRIVATE KEY-----";
        let (masked, findings) = obfuscate(source, &mut vault, &Options::default(), "test");
        for kind in ["CPR", "IBAN", "PAYMENT_CARD", "PEM_PRIVATE_KEY"] {
            assert!(
                findings.iter().any(|finding| finding.kind == kind),
                "{kind}"
            );
        }
        assert!(!masked.contains("BEGIN PRIVATE KEY"));
        assert_eq!(rehydrate(&masked, &vault), source);
    }

    #[test]
    fn flag_and_off_modes_preserve_bytes() {
        let source = "jane@company.dk";
        let flag = Options {
            mode: Mode::Flag,
            ..Options::default()
        };
        let mut vault = Vault::default();
        let (flagged, findings) = obfuscate(source, &mut vault, &flag, "prompt");
        assert_eq!(flagged, source);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].replaced);
        let off_options = Options {
            mode: Mode::Off,
            ..Options::default()
        };
        let (off, findings) = obfuscate(source, &mut vault, &off_options, "prompt");
        assert_eq!(off, source);
        assert!(findings.is_empty());
    }

    #[test]
    fn rejects_duplicate_and_malformed_vault_rows() {
        let entry = VaultEntry {
            value: "secret".into(),
            placeholder: "ZIRV_SECRET_TOKEN_1".into(),
            kind: "TOKEN".into(),
            class: ValueClass::Secret,
            first_seen_surface: "test".into(),
        };
        assert!(Vault::from_entries(vec![entry.clone(), entry]).is_err());
        let malformed = VaultEntry {
            placeholder: "bad".into(),
            ..VaultEntry {
                value: "secret".into(),
                placeholder: String::new(),
                kind: "TOKEN".into(),
                class: ValueClass::Secret,
                first_seen_surface: "test".into(),
            }
        };
        assert!(Vault::from_entries(vec![malformed]).is_err());
    }
}
