//! Per-seat profile store (#537): the final proxy decision, keyed by the seat's stable short id, so a
//! rollover or handover successor re-renders the same `[zirv proxy]` layer. Rows in
//! `proxy-decisions.jsonl` are repo-keyed and last-wins, which cannot answer "this seat's profile".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::decision::{ProxyDecision, ProxyOverride};
use crate::commands::ctx::{CtxResult, sessions, state};

const PROFILE_DIR: &str = "proxy-profile";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredProfile {
    pub decision: ProxyDecision,
    #[serde(default)]
    pub operator_override: Option<ProxyOverride>,
    #[serde(default)]
    pub started_workflow_id: Option<String>,
}

pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join(PROFILE_DIR)
}

/// Writes the profile for `session`'s seat; best-effort callers ignore the error.
pub fn save(state_dir: &Path, session: &str, profile: &StoredProfile) -> CtxResult<()> {
    let short = sessions::short_id(session);
    if short.is_empty() {
        return Ok(());
    }
    let dir = dir(state_dir);
    state::create_private_dir_all(&dir)?;
    state::write_atomic_private(
        &dir,
        &format!("{short}.json"),
        &serde_json::to_string_pretty(profile)?,
    )?;
    Ok(())
}

/// A missing, corrupt or old-format file is `None`, never an error.
pub fn load(state_dir: &Path, session: &str) -> Option<StoredProfile> {
    let short = sessions::short_id(session);
    if short.is_empty() {
        return None;
    }
    let text = std::fs::read_to_string(dir(state_dir).join(format!("{short}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every readable stored profile, newest first is not promised; unreadable files are skipped.
pub fn load_all(state_dir: &Path) -> Vec<(String, StoredProfile)> {
    let Ok(entries) = std::fs::read_dir(dir(state_dir)) else {
        return Vec::new();
    };
    let mut found: Vec<(String, StoredProfile)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let short = path.file_stem()?.to_str()?.to_string();
            let profile = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
            Some((short, profile))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The layer a successor of `session`'s seat must carry, or `None` when this seat never had a proxy decision.
pub fn layer_for_session(state_dir: &Path, session: &str) -> Option<String> {
    let profile = load(state_dir, session)?;
    Some(super::prompt_layer(
        &profile.decision,
        profile.started_workflow_id.as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision() -> ProxyDecision {
        super::super::tests::sample_decision()
    }

    #[test]
    fn a_saved_profile_round_trips_and_renders_the_same_layer() {
        let tmp = tempfile::tempdir().expect("tmp");
        let profile = StoredProfile {
            decision: decision(),
            operator_override: Some(ProxyOverride {
                review: Some(true),
                ..ProxyOverride::default()
            }),
            started_workflow_id: Some("wf-1".to_string()),
        };
        save(tmp.path(), "abcd1234-0000-4000-8000-000000000000", &profile).expect("save");
        assert_eq!(load(tmp.path(), "abcd1234-ffff").as_ref(), Some(&profile));
        assert_eq!(
            layer_for_session(tmp.path(), "abcd1234"),
            Some(super::super::prompt_layer(&profile.decision, Some("wf-1")))
        );
    }

    #[test]
    fn a_corrupt_or_old_format_file_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir(tmp.path())).expect("dir");
        std::fs::write(dir(tmp.path()).join("deadbeef.json"), "{not json").expect("write");
        std::fs::write(dir(tmp.path()).join("cafe0000.json"), "{\"decision\":{}}").expect("write");
        assert!(load(tmp.path(), "deadbeef").is_none());
        assert!(load(tmp.path(), "cafe0000").is_none());
        assert!(load(tmp.path(), "missing0").is_none());
        assert!(load_all(tmp.path()).is_empty());
        assert!(layer_for_session(tmp.path(), "deadbeef").is_none());
    }

    /// The recorded profile survives the store, a second successor and an old-format file with the layer byte-identical. (#537)
    #[test]
    fn the_recorded_profile_fixture_survives_successive_successors_intact() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/proxy/profile-survival.json");
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
        let expected = fixture["expected_layer"]
            .as_str()
            .expect("layer")
            .to_string();
        let profile: StoredProfile =
            serde_json::from_value(fixture["profile"].clone()).expect("profile");
        assert_eq!(
            profile.operator_override.and_then(|ov| ov.execution),
            Some(crate::commands::workflow::profile::ExecutionMode::Direct)
        );

        let tmp = tempfile::tempdir().expect("tmp");
        save(tmp.path(), "feed0001-aaaa", &profile).expect("save");
        // Successor one and two address the same seat short id and read back the identical profile.
        for successor in ["feed0001-bbbb", "feed0001-cccc"] {
            let loaded = load(tmp.path(), successor).expect("profile survives");
            assert_eq!(loaded, profile);
            assert_eq!(
                layer_for_session(tmp.path(), successor),
                Some(expected.clone())
            );
        }

        // A profile written before the optional fields existed still loads.
        let mut old = fixture["profile"].clone();
        let object = old.as_object_mut().expect("object");
        object.remove("operator_override");
        object.remove("started_workflow_id");
        std::fs::write(dir(tmp.path()).join("0ld00000.json"), old.to_string()).expect("write");
        assert_eq!(layer_for_session(tmp.path(), "0ld00000"), Some(expected));
    }
}
