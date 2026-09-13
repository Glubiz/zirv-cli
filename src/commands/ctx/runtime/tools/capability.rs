//! Typed arguments for the configured-capability tools (issue #483, roadmap
//! N14): MCP, web, browser, diagnostics, artifacts and frontend.
//!
//! These parse exactly like N05's file and process tools -- a closed schema, a
//! complete typed request, no filesystem or network reach until the N04 broker
//! has admitted an action. They live beside `files.rs` and `process.rs`
//! because they are the same kind of thing: the only difference is which
//! service the admitted action finally reaches.
//!
//! Two shapes are deliberate.
//!
//! Evidence paths are zirv-chosen. A browser capture names a *label*, not a
//! path: the tool slugifies it and writes under a state-dir evidence root, the
//! same way the output store hands back opaque ids instead of accepting a
//! caller path. Provider output therefore cannot steer where a screenshot
//! lands.
//!
//! MCP tool names are namespaced `mcp__<server>__<tool>` when a small
//! catalogue is inlined into the registry, so a server can never shadow a
//! built-in tool name no matter what it calls its tools.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{ToolError, ToolErrorCode};

pub(super) const MCP_PREFIX: &str = "mcp__";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WebSearchArgs {
    pub query: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WebFetchArgs {
    pub url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserCaptureArgs {
    pub url: String,
    /// A human label for the capture. Slugified into a zirv-owned evidence
    /// path; never used as a path itself.
    pub label: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    800
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserInspectArgs {
    pub url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpListArgs {
    /// Limits the index to one configured server. Absent means every one.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpDescribeArgs {
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct McpCallArgs {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactRegisterArgs {
    pub path: PathBuf,
    #[serde(default)]
    pub kind: Option<ArtifactKindArg>,
    #[serde(default)]
    pub workflow_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ArtifactKindArg {
    Image,
    Svg,
    Html,
    Diagram,
    Document,
    Other,
}

impl ArtifactKindArg {
    pub(super) fn kind(self) -> crate::commands::workflow::artifact::ArtifactKind {
        use crate::commands::workflow::artifact::ArtifactKind;
        match self {
            Self::Image => ArtifactKind::Image,
            Self::Svg => ArtifactKind::Svg,
            Self::Html => ArtifactKind::Html,
            Self::Diagram => ArtifactKind::Diagram,
            Self::Document => ArtifactKind::Document,
            Self::Other => ArtifactKind::Other,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactPresentArgs {
    pub id: String,
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrontendReviewArgs {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Turns a caller label into one safe path segment. Everything outside
/// `[a-z0-9-]` becomes a dash, so no separator, drive letter, `..` or NUL can
/// survive into a path.
pub(super) fn evidence_slug(label: &str) -> String {
    let mut slug = String::with_capacity(label.len());
    for character in label.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(64);
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "capture".to_string()
    } else {
        trimmed
    }
}

/// The zirv-owned root every browser evidence file lands under. Inside the
/// state directory, per repository slug, so a capture is findable from the
/// CLI and from a headless result without the model choosing where it went.
pub(super) fn evidence_root(state: &crate::commands::ctx::state::StateDir, repo: &Path) -> PathBuf {
    state
        .root()
        .join("native-evidence")
        .join(crate::commands::ctx::state::repo_slug(repo))
}

/// Parses an absolute http(s) URL into a broker network target, so the host
/// crosses the same allowlist an operator configured for every other effect.
pub(super) fn network_target(
    url: &str,
) -> Result<crate::commands::ctx::runtime::enforcement::NetworkTarget, ToolError> {
    use crate::commands::ctx::runtime::capabilities::host_of;

    let scheme = if url.starts_with("https://") {
        "https"
    } else if url.starts_with("http://") {
        "http"
    } else {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{url:?} must be an absolute http:// or https:// URL"),
        ));
    };
    let host = host_of(url).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{url:?} has no host"),
        )
    })?;
    crate::commands::ctx::runtime::enforcement::NetworkTarget::new(scheme, &host, None)
        .map_err(|error| ToolError::new(ToolErrorCode::InvalidArguments, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_evidence_label_can_never_escape_its_own_directory() {
        for label in ["../../etc/passwd", "C:\\windows\\system32", "a/b/c", "\0"] {
            let slug = evidence_slug(label);
            assert!(!slug.contains(['/', '\\', '\0']), "{slug}");
            assert!(!slug.contains(".."), "{slug}");
        }
        assert_eq!(evidence_slug("Home page — wide"), "home-page-wide");
        assert_eq!(evidence_slug("---"), "capture");
    }

    #[test]
    fn only_absolute_http_urls_become_a_network_target() {
        assert!(network_target("https://docs.example/a").is_ok());
        assert!(network_target("http://docs.example").is_ok());
        for bad in ["file:///etc/passwd", "docs.example", "https://"] {
            assert!(network_target(bad).is_err(), "{bad} must be refused");
        }
    }
}
