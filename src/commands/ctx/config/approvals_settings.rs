use serde::Deserialize;

use super::{CtxResult, EnvLookup};

/// Longest `hold_secs`: the installed hook timeout is `hold_secs + 30` and must stay below Claude's own 600 s ceiling.
pub const MAX_HOLD_SECS: u64 = 570;

/// Operator-only approvals inbox (#840). Off by default; a repository cannot turn it on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalsConfig {
    /// Hold a Claude `PermissionRequest` for the owning dashboard's operator instead of showing the pane dialog at once.
    pub inbox: bool,
    /// Seconds a hook holds a request before the native pane dialog shows.
    pub hold_secs: u64,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            inbox: false,
            hold_secs: 300,
        }
    }
}

impl ApprovalsConfig {
    /// The hook installed in Claude's launch settings waits this long before Claude itself gives up.
    pub fn hook_timeout_secs(&self) -> u64 {
        self.hold_secs + 30
    }

    /// Operator file plus `ZIRV_CTX_APPROVALS_*` only, for the permission hook's hot path (no repo layer, no full config load).
    pub fn load_operator_only(env: EnvLookup<'_>) -> CtxResult<Self> {
        let text = match super::operator_path().map(std::fs::read_to_string) {
            Ok(Ok(text)) => text,
            _ => String::new(),
        };
        let mut cfg = Self::default();
        if text.contains("approvals") {
            let mut table: toml::Table = toml::from_str(&text)?;
            if let Some(value) = table.remove("approvals") {
                cfg = value.try_into()?;
            }
        }
        if let Some(raw) = env("ZIRV_CTX_APPROVALS_INBOX") {
            cfg.inbox = matches!(raw.trim(), "true" | "1");
        }
        if let Some(raw) = env("ZIRV_CTX_APPROVALS_HOLD_SECS") {
            cfg.hold_secs = raw.trim().parse()?;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub(super) fn validate(&self) -> CtxResult<()> {
        if self.hold_secs == 0 || self.hold_secs > MAX_HOLD_SECS {
            return Err(format!(
                "approvals.hold_secs must be between 1 and {MAX_HOLD_SECS}, got {}",
                self.hold_secs
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_default_off_with_a_five_minute_hold() {
        let cfg = ApprovalsConfig::default();
        assert!(!cfg.inbox);
        assert_eq!(cfg.hold_secs, 300);
        assert_eq!(cfg.hook_timeout_secs(), 330);
    }

    #[test]
    fn hold_secs_outside_the_hook_ceiling_is_rejected() {
        for bad in [0, MAX_HOLD_SECS + 1] {
            let cfg = ApprovalsConfig {
                inbox: true,
                hold_secs: bad,
            };
            assert!(cfg.validate().is_err(), "{bad} must be rejected");
        }
        assert!(
            ApprovalsConfig {
                inbox: true,
                hold_secs: MAX_HOLD_SECS
            }
            .validate()
            .is_ok()
        );
    }
}
