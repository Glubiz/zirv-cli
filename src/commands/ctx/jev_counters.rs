//! Per-gate funnel counters for the `[jev]` gates, so an enabled gate that never fires is
//! distinguishable from a broken one in `zirv ctx jev status`.
//!
//! One row per event (`{ts, v, gate, stage, reason?}`) appended with O_APPEND to a per-day file
//! under `jev-counters/`: concurrent hook processes never lose or tear a row, counts are exact,
//! and bounding is deleting day files past [`RETAIN_DAYS`]. Rows carry no text, only the gate
//! key, a stage name and a static reason code. A gate that is off never reaches the write.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use serde::Deserialize;

use crate::commands::ctx::config::JevConfig;
use crate::commands::ctx::state::{self, StateDir};

/// The zirv version stamped on every Jev ledger row.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

const COUNTERS_DIR: &str = "jev-counters";
const RETAIN_DAYS: u64 = 30;
pub(crate) const STATUS_WINDOW_DAYS: u64 = 14;
const DAY_SECS: u64 = 86_400;

/// How far a gated call got. `Reached` is every pass through a call site; an early return
/// carries a reason code with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Reached,
    Eligible,
    SentNet,
    SentCache,
    Answered,
    Acted,
    /// An early return, always with a reason code.
    Exit,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Reached => "reached",
            Stage::Eligible => "eligible",
            Stage::SentNet => "sent_net",
            Stage::SentCache => "sent_cache",
            Stage::Answered => "answered",
            Stage::Acted => "acted",
            Stage::Exit => "exit",
        }
    }
}

/// The `[jev]` gate key a decision-log site name belongs to (several sites share a gate).
pub(crate) fn gate_for_site(site: &str) -> &str {
    match site {
        "judge" | "crash" => "supervisor",
        "intake_plan" | "intake_clarification" => "intake_savings",
        "workflow-review-reuse" => "review_reuse",
        "harvest" => "harvest_screen",
        "context-parent-reports" | "context-skill-descriptions" => "context",
        "workflow-review-disposition" | "workflow-review-dedup" => "review",
        "workflow-gate-reclassification" | "workflow-artifact-substance" => "gates",
        other => other,
    }
}

/// Append one counter row for `gate`. `enabled` false returns before any file work.
pub(crate) fn count(
    state: &StateDir,
    gate: &str,
    enabled: bool,
    stage: Stage,
    reason: Option<&'static str>,
) {
    if !enabled {
        return;
    }
    let now = state::now_secs();
    let dir = state.root().join(COUNTERS_DIR);
    let path = dir.join(format!("{}.jsonl", now / DAY_SECS));
    if !path.exists() {
        if state::create_private_dir_all(&dir).is_err() {
            return;
        }
        prune(&dir, now / DAY_SECS);
    }
    let row = serde_json::json!({
        "ts": now,
        "v": VERSION,
        "gate": gate,
        "stage": stage.as_str(),
        "reason": reason,
    });
    if let Ok(mut file) = state::open_private_append(&path) {
        let _ = append_row(&mut file, &row);
    }
}

/// One row is exactly one `write`: an `O_APPEND` write of a single buffer is not interleaved with
/// another process's, which several `write` calls (as `writeln!` of a `Value` makes) would be.
fn append_row(out: &mut impl Write, row: &serde_json::Value) -> std::io::Result<()> {
    let mut line = row.to_string();
    line.push('\n');
    out.write_all(line.as_bytes())
}

/// The day number a counter file is named for.
fn day_of(path: &Path) -> Option<u64> {
    path.file_stem()?.to_str()?.parse().ok()
}

fn prune(dir: &Path, today: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if day_of(&entry.path()).is_some_and(|day| day + RETAIN_DAYS < today) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub(crate) struct GateCounts {
    pub reached: u64,
    pub eligible: u64,
    pub sent_net: u64,
    pub sent_cache: u64,
    pub answered: u64,
    pub acted: u64,
    /// Early-return reason code to count.
    pub exits: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct CounterRow {
    gate: String,
    stage: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Exact counts per gate over the last [`STATUS_WINDOW_DAYS`] day files.
pub(crate) fn rollup(state: &StateDir, now: u64) -> BTreeMap<String, GateCounts> {
    let mut gates: BTreeMap<String, GateCounts> = BTreeMap::new();
    let first = (now / DAY_SECS).saturating_sub(STATUS_WINDOW_DAYS - 1);
    let Ok(entries) = std::fs::read_dir(state.root().join(COUNTERS_DIR)) else {
        return gates;
    };
    for entry in entries.flatten() {
        if !day_of(&entry.path()).is_some_and(|day| day >= first) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for row in text
            .lines()
            .filter_map(|line| serde_json::from_str::<CounterRow>(line).ok())
        {
            let counts = gates.entry(row.gate).or_default();
            match row.stage.as_str() {
                "reached" => counts.reached += 1,
                "eligible" => counts.eligible += 1,
                "sent_net" => counts.sent_net += 1,
                "sent_cache" => counts.sent_cache += 1,
                "answered" => counts.answered += 1,
                "acted" => counts.acted += 1,
                "exit" => {
                    let reason = row.reason.unwrap_or_else(|| "unspecified".to_string());
                    *counts.exits.entry(reason).or_default() += 1;
                }
                _ => {}
            }
        }
    }
    gates
}

/// Versions other than `running` that wrote any Jev ledger row in the window. Rows from before
/// version stamping carry none and say nothing.
pub(crate) fn foreign_versions(state: &StateDir, now: u64, running: &str) -> BTreeSet<String> {
    #[derive(Deserialize)]
    struct Stamped {
        #[serde(default)]
        ts: u64,
        #[serde(default)]
        v: Option<String>,
    }
    let cutoff = now.saturating_sub(STATUS_WINDOW_DAYS * DAY_SECS);
    let mut files = vec![
        state.root().join(super::jev::JEV_DECISIONS_FILE),
        state.root().join(super::jev::JEV_SKIPS_FILE),
        state.root().join(super::jev::JEV_EFFECTS_FILE),
    ];
    if let Ok(entries) = std::fs::read_dir(state.root().join(COUNTERS_DIR)) {
        files.extend(entries.flatten().map(|entry| entry.path()));
    }
    let mut found = BTreeSet::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for row in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Stamped>(line).ok())
        {
            if let Some(v) = row.v
                && row.ts >= cutoff
                && v != running
            {
                found.insert(v);
            }
        }
    }
    found
}

/// The version of the zirv source tree at `dir`, when `dir` holds its `Cargo.toml`.
pub(crate) fn source_tree_version(dir: &Path) -> Option<String> {
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(dir.join("Cargo.toml")).ok()?).ok()?;
    let package = manifest.get("package")?;
    if package.get("name")?.as_str()? != "zirv" {
        return None;
    }
    package.get("version")?.as_str().map(str::to_string)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Label {
    Active,
    Retired,
    Falsified,
    Unvalidated,
    Dormant,
}

impl Label {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Label::Active => "active",
            Label::Retired => "retired",
            Label::Falsified => "falsified",
            Label::Unvalidated => "unvalidated",
            Label::Dormant => "dormant",
        }
    }
}

/// A gate's static label and the hook or command path it waits on. `Falsified` and `Retired`
/// are static; the rest of the label is computed from `on` and the window's counts.
struct GateInfo {
    key: &'static str,
    fixed: Option<(Label, &'static str)>,
    waits_on: &'static str,
}

const FALSIFIED_NOTE: &str =
    "falsified on the operator's recorded data 2026-10-07 (E-heuristics.md, I-history.md, V1, V4)";

const GATES: [GateInfo; 21] = [
    GateInfo {
        key: "memory",
        fixed: Some((
            Label::Retired,
            "memory rerank and harvest-gate sites were removed",
        )),
        waits_on: "nothing: the key is parsed and ignored",
    },
    GateInfo {
        key: "supervisor",
        fixed: None,
        waits_on: "`zirv ctx loop` judge (doomed-run check, falsified 2026-10-07 E2), task-card crash triage",
    },
    GateInfo {
        key: "dispatch",
        fixed: None,
        waits_on: "PreToolUse Agent dispatch hook (only a seat with an omitted model)",
    },
    GateInfo {
        key: "review",
        fixed: None,
        waits_on: "`zirv workflow review run` disposition and dedup checks",
    },
    GateInfo {
        key: "gates",
        fixed: None,
        waits_on: "workflow gate reclassification and artifact-substance check on stage advance",
    },
    GateInfo {
        key: "context",
        fixed: None,
        waits_on: "ctx compile of parent reports and skill descriptions",
    },
    GateInfo {
        key: "intake_savings",
        fixed: None,
        waits_on: "workflow team plan compile (orchestrated, Substantial plans with a planner seat)",
    },
    GateInfo {
        key: "review_reuse",
        fixed: None,
        waits_on: "`zirv workflow review run` reuse check",
    },
    GateInfo {
        key: "harvest_screen",
        fixed: None,
        waits_on: "memory harvest at session end",
    },
    GateInfo {
        key: "admin_dispatch",
        fixed: None,
        waits_on: "UserPromptSubmit admin-dispatch check",
    },
    GateInfo {
        key: "approve",
        fixed: Some((Label::Falsified, FALSIFIED_NOTE)),
        waits_on: "PermissionRequest and safety hook on Bash asks",
    },
    GateInfo {
        key: "approve_allow",
        fixed: Some((Label::Falsified, FALSIFIED_NOTE)),
        waits_on: "PermissionRequest and safety hook on unmatched-default Bash asks (needs approve)",
    },
    GateInfo {
        key: "classify",
        fixed: None,
        waits_on: "workflow classification of an unrouted task",
    },
    GateInfo {
        key: "handoff_select",
        fixed: None,
        waits_on: "handoff distillation on restart or handover",
    },
    GateInfo {
        key: "compaction_select",
        fixed: None,
        waits_on: "compaction focus text build",
    },
    GateInfo {
        key: "inject_screen",
        fixed: None,
        waits_on: "screening of untrusted input before injection",
    },
    GateInfo {
        key: "inject",
        fixed: None,
        waits_on: "automatic compact, restart, mail and Stop injections",
    },
    GateInfo {
        key: "stop_verify",
        fixed: Some((Label::Falsified, FALSIFIED_NOTE)),
        waits_on: "Stop hook completion-claim check",
    },
    GateInfo {
        key: "missing_tests",
        fixed: None,
        waits_on: "Stop hook missing-tests gate",
    },
    GateInfo {
        key: "launch_effort",
        fixed: Some((Label::Falsified, FALSIFIED_NOTE)),
        waits_on: "first turn of a headless launch",
    },
    GateInfo {
        key: "retry",
        fixed: None,
        waits_on: "PostToolUseFailure hook after repeated identical tool failures",
    },
];

/// One `[jev]` key's row in `zirv ctx jev status`.
pub(crate) struct GateRow {
    pub key: &'static str,
    pub on: bool,
    pub label: Label,
    pub note: Option<&'static str>,
    /// Set only for an ON gate with zero reach in the window.
    pub idle_reason: Option<&'static str>,
    pub counts: GateCounts,
}

/// Every `[jev]` key (the deprecated `memory` included) with its label and window counts.
/// `Dormant` is computed: on, and never reached in the window.
pub(crate) fn gate_rows(cfg: &JevConfig, counts: &BTreeMap<String, GateCounts>) -> Vec<GateRow> {
    GATES
        .iter()
        .map(|info| {
            let on = (info.key == "memory" && cfg.memory)
                || super::jev::gate_list(cfg)
                    .iter()
                    .any(|(name, gate_on)| *name == info.key && *gate_on);
            let counts = counts.get(info.key).cloned().unwrap_or_default();
            let (label, note) = match info.fixed {
                Some((label, note)) => (label, Some(note)),
                None if on && counts.reached == 0 => (Label::Dormant, None),
                None if on && counts.acted > 0 => (Label::Active, None),
                None => (Label::Unvalidated, None),
            };
            let idle_reason = (on && label == Label::Dormant).then_some(info.waits_on);
            GateRow {
                key: info.key,
                on,
                label,
                note,
                idle_reason,
                counts,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        (dir, state)
    }

    #[test]
    fn counts_are_exact_under_repeated_calls_and_carry_reasons_and_version() {
        let (dir, state) = state();
        for _ in 0..7 {
            count(&state, "review", true, Stage::Reached, None);
        }
        count(&state, "review", true, Stage::Exit, Some("no_credential"));
        count(&state, "review", true, Stage::SentCache, None);
        let counts = rollup(&state, state::now_secs());
        let review = &counts["review"];
        assert_eq!(review.reached, 7);
        assert_eq!(review.sent_cache, 1);
        assert_eq!(review.exits["no_credential"], 1);
        let day = std::fs::read_dir(dir.path().join(COUNTERS_DIR))
            .expect("dir")
            .next()
            .expect("day file")
            .expect("entry")
            .path();
        let first = std::fs::read_to_string(day).expect("read");
        let row: serde_json::Value =
            serde_json::from_str(first.lines().next().expect("row")).expect("json");
        assert_eq!(row["v"], VERSION);
    }

    #[test]
    fn one_row_is_exactly_one_write_call() {
        struct CountingWriter {
            writes: Vec<Vec<u8>>,
        }
        impl Write for CountingWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.writes.push(buf.to_vec());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut out = CountingWriter { writes: Vec::new() };
        append_row(
            &mut out,
            &serde_json::json!({"gate": "review", "stage": "reached"}),
        )
        .expect("write");
        assert_eq!(out.writes.len(), 1, "{:?}", out.writes);
        assert!(out.writes[0].ends_with(b"\n"));
    }

    #[test]
    fn an_off_gate_writes_nothing() {
        let (dir, state) = state();
        count(&state, "review", false, Stage::Reached, None);
        assert!(!dir.path().join(COUNTERS_DIR).exists());
    }

    #[test]
    fn day_files_past_retention_are_pruned_when_a_new_day_starts() {
        let (dir, state) = state();
        let counters = dir.path().join(COUNTERS_DIR);
        std::fs::create_dir_all(&counters).expect("dir");
        let old = counters.join("1.jsonl");
        std::fs::write(&old, "{}\n").expect("old");
        count(&state, "review", true, Stage::Reached, None);
        assert!(!old.exists());
    }

    #[test]
    fn an_on_gate_with_zero_reach_is_dormant_with_its_idle_reason() {
        let cfg = JevConfig {
            review: true,
            approve: true,
            ..JevConfig::default()
        };
        let mut counts = BTreeMap::new();
        counts.insert(
            "approve".to_string(),
            GateCounts {
                reached: 3,
                ..GateCounts::default()
            },
        );
        let rows = gate_rows(&cfg, &counts);
        let review = rows.iter().find(|row| row.key == "review").expect("review");
        assert_eq!(review.label, Label::Dormant);
        assert!(
            review
                .idle_reason
                .is_some_and(|reason| reason.contains("review run"))
        );
        let approve = rows
            .iter()
            .find(|row| row.key == "approve")
            .expect("approve");
        assert_eq!(approve.label, Label::Falsified);
        assert!(approve.idle_reason.is_none());
        let off = rows
            .iter()
            .find(|row| row.key == "dispatch")
            .expect("dispatch");
        assert_eq!(off.label, Label::Unvalidated);
        assert_eq!(rows.len(), 21);
        assert_eq!(
            rows.iter()
                .find(|row| row.key == "memory")
                .map(|row| row.label),
            Some(Label::Retired)
        );
    }

    #[test]
    fn the_retired_memory_key_still_parses_and_is_labelled_retired() {
        let cfg: JevConfig = toml::from_str("memory = true").expect("legacy key parses");
        let rows = gate_rows(&cfg, &BTreeMap::new());
        let memory = rows.iter().find(|row| row.key == "memory").expect("memory");
        assert!(memory.on);
        assert_eq!(memory.label, Label::Retired);
        assert!(memory.idle_reason.is_none());
    }

    #[test]
    fn a_row_written_by_another_version_is_reported() {
        let (dir, state) = state();
        let now = state::now_secs();
        std::fs::write(
            dir.path().join(super::super::jev::JEV_DECISIONS_FILE),
            format!(
                "{{\"site\":\"x\",\"ts\":{now},\"v\":\"0.0.1\"}}\n{{\"site\":\"x\",\"ts\":{now}}}\n"
            ),
        )
        .expect("write");
        count(&state, "review", true, Stage::Reached, None);
        let found = foreign_versions(&state, now, VERSION);
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec!["0.0.1".to_string()]
        );
    }

    #[test]
    fn source_tree_version_reads_only_the_zirv_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(source_tree_version(dir.path()), None);
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"zirv\"\nversion = \"9.9.9\"\n",
        )
        .expect("manifest");
        assert_eq!(source_tree_version(dir.path()).as_deref(), Some("9.9.9"));
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"other\"\nversion = \"1.0.0\"\n",
        )
        .expect("manifest");
        assert_eq!(source_tree_version(dir.path()), None);
    }
}
