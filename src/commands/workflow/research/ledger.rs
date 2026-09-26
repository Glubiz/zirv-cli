//! `lock.json` (written once, immutable) and `ledger.jsonl` (append + flush
//! per event, monotonic `seq`) -- the campaign's durable record. Resume
//! replays this file; it is the only source of truth for "what already
//! happened" across a process restart. See issue #802's ledger contract.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::manifest::Manifest;
use crate::commands::ctx::CtxResult;
#[cfg(test)]
use crate::commands::ctx::state::now_secs;

pub const LOCK_FILE: &str = "lock.json";
pub const LEDGER_FILE: &str = "ledger.jsonl";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    pub manifest: Manifest,
    pub manifest_sha256: String,
    pub baseline_sha: String,
    pub evaluator_version: Option<String>,
    pub evaluator_files: BTreeMap<String, String>,
    pub evaluator_fingerprint: String,
    pub zirv_version: String,
    pub price_as_of: Option<String>,
    pub started_at: u64,
}

impl Lock {
    pub fn path(campaign_dir: &Path) -> PathBuf {
        campaign_dir.join(LOCK_FILE)
    }

    pub fn write(&self, campaign_dir: &Path) -> CtxResult<()> {
        let path = Self::path(campaign_dir);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)
            .map_err(|err| format!("could not write '{}': {err}", path.display()))?;
        Ok(())
    }

    pub fn read(campaign_dir: &Path) -> CtxResult<Self> {
        let path = Self::path(campaign_dir);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("could not read '{}': {err}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn exists(campaign_dir: &Path) -> bool {
        Self::path(campaign_dir).is_file()
    }
}

pub fn manifest_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LedgerEvent {
    CampaignStarted {
        seq: u64,
        ts: u64,
        manifest_sha256: String,
        baseline_sha: String,
    },
    TrialScheduled {
        seq: u64,
        ts: u64,
        trial_id: String,
        candidate: String,
        arm: String,
        stage: String,
        task: String,
        rep: u32,
        split: String,
        attempt: u32,
        reserved_spend_usd: f64,
        reserved_calls: u64,
    },
    TrialFinished {
        seq: u64,
        ts: u64,
        trial_id: String,
        status: String,
        correctness: Option<f64>,
        quality: Option<f64>,
        cost_usd: Option<f64>,
        cost_complete: bool,
        overhead_usd: f64,
        wall_ms: u64,
        receipts: BTreeMap<String, u64>,
    },
    TrialFailed {
        seq: u64,
        ts: u64,
        trial_id: String,
        reason: String,
        charged_usd: f64,
        attempt: u32,
        retryable: bool,
    },
    CandidateProposed {
        seq: u64,
        ts: u64,
        candidate: String,
        hypothesis: String,
    },
    CandidateRejected {
        seq: u64,
        ts: u64,
        candidate: String,
        reason: String,
    },
    StageDecision {
        seq: u64,
        ts: u64,
        candidate: String,
        stage: String,
        verdict: String,
        detail: serde_json::Value,
    },
    HoldoutUsed {
        seq: u64,
        ts: u64,
        corpus_file: String,
        corpus_version: String,
        uses: u32,
    },
    CampaignStopped {
        seq: u64,
        ts: u64,
        reason: String,
    },
    CampaignFinished {
        seq: u64,
        ts: u64,
        promoted: Option<String>,
    },
}

impl LedgerEvent {
    pub fn seq(&self) -> u64 {
        match self {
            LedgerEvent::CampaignStarted { seq, .. }
            | LedgerEvent::TrialScheduled { seq, .. }
            | LedgerEvent::TrialFinished { seq, .. }
            | LedgerEvent::TrialFailed { seq, .. }
            | LedgerEvent::CandidateProposed { seq, .. }
            | LedgerEvent::CandidateRejected { seq, .. }
            | LedgerEvent::StageDecision { seq, .. }
            | LedgerEvent::HoldoutUsed { seq, .. }
            | LedgerEvent::CampaignStopped { seq, .. }
            | LedgerEvent::CampaignFinished { seq, .. } => *seq,
        }
    }
}

pub struct Ledger {
    path: PathBuf,
    next_seq: u64,
}

impl Ledger {
    pub fn path(campaign_dir: &Path) -> PathBuf {
        campaign_dir.join(LEDGER_FILE)
    }

    /// Opens the ledger for appending, replaying whatever is already there
    /// to seed `next_seq`. Never truncates -- `run`'s caller decides whether
    /// a fresh campaign is even allowed to reuse this directory.
    pub fn open(campaign_dir: &Path) -> CtxResult<(Self, Vec<LedgerEvent>)> {
        let path = Self::path(campaign_dir);
        let events = replay(&path)?;
        let next_seq = events.last().map(|e| e.seq() + 1).unwrap_or(0);
        Ok((Self { path, next_seq }, events))
    }

    pub fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    pub fn append(&self, event: &LedgerEvent) -> CtxResult<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|err| format!("could not open '{}': {err}", self.path.display()))?;
        let line = serde_json::to_string(event)?;
        writeln!(file, "{line}")
            .map_err(|err| format!("could not append to '{}': {err}", self.path.display()))?;
        file.flush()
            .map_err(|err| format!("could not flush '{}': {err}", self.path.display()))?;
        Ok(())
    }
}

/// Replays every well-formed line in order. A torn trailing write (the only
/// way a partial line can occur, since every append is flushed) is dropped
/// rather than failing the whole replay.
pub fn replay(path: &Path) -> CtxResult<Vec<LedgerEvent>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LedgerEvent>(line) {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }
    Ok(events)
}

#[cfg(test)]
pub fn timestamp() -> u64 {
    now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_replay_round_trips_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, seeded) = Ledger::open(dir.path()).unwrap();
        assert!(seeded.is_empty());

        ledger
            .append(&LedgerEvent::CampaignStarted {
                seq: 0,
                ts: timestamp(),
                manifest_sha256: "abc".into(),
                baseline_sha: "def".into(),
            })
            .unwrap();
        ledger
            .append(&LedgerEvent::CampaignStopped {
                seq: 1,
                ts: timestamp(),
                reason: "budget_exhausted:spend".into(),
            })
            .unwrap();

        let replayed = replay(&Ledger::path(dir.path())).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].seq(), 0);
        assert_eq!(replayed[1].seq(), 1);
    }

    #[test]
    fn reopening_seeds_next_seq_from_the_existing_log() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, _) = Ledger::open(dir.path()).unwrap();
        ledger
            .append(&LedgerEvent::CampaignStarted {
                seq: 0,
                ts: timestamp(),
                manifest_sha256: "abc".into(),
                baseline_sha: "def".into(),
            })
            .unwrap();

        let (mut reopened, events) = Ledger::open(dir.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(reopened.next_seq(), 1);
    }

    #[test]
    fn a_torn_trailing_line_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = Ledger::path(dir.path());
        std::fs::write(
            &path,
            "{\"event\":\"campaign_started\",\"seq\":0,\"ts\":1,\"manifest_sha256\":\"a\",\"baseline_sha\":\"b\"}\n{\"event\":\"trial_sch",
        )
        .unwrap();
        let events = replay(&path).unwrap();
        assert_eq!(events.len(), 1);
    }
}
