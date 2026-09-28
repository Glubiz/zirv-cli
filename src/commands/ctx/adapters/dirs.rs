//! Shared home/state directory resolution used by every adapter.
use super::*;

/// Shared `home_dir()` body for every adapter: an operator-forced `home`
/// wins, else the real platform home, else `.` as a last resort.
pub(in crate::commands::ctx) fn resolve_home_dir(forced_home: &Option<PathBuf>) -> PathBuf {
    forced_home
        .clone()
        .or_else(|| crate::utils::home_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Shared `#[cfg(test)]` `state_dir()` body: the test seam pins the zirv
/// state root instead of resolving the real platform state directory, so a
/// test that never opts in cannot reach -- or write into -- the developer's
/// own state dir.
#[cfg(test)]
pub(in crate::commands::ctx) fn resolve_state_dir(
    forced_state_root: &Option<PathBuf>,
) -> Option<super::super::state::StateDir> {
    forced_state_root
        .clone()
        .map(super::super::state::StateDir::from_root)
}

/// Shared non-test `state_dir()` body: resolves the real platform state
/// directory from the process environment.
#[cfg(not(test))]
pub(in crate::commands::ctx) fn resolve_state_dir() -> Option<super::super::state::StateDir> {
    super::super::state::StateDir::resolve(&super::super::config::env_from_process()).ok()
}
