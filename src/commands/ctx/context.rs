//! The canonical `.zirv/context/` instruction layer (issue #41): one place a
//! project's AI working instructions live once for every Zirv-launched
//! harness, with optional harness-specific additions layered on top.
//! `super::optimize::collect_surfaces` reads it through `Layer::ContextCommon`/
//! `ContextClaude`/`ContextCodex`; this module owns the file locations and
//! the deterministic precedence rule between this layer and the native
//! instruction files (CLAUDE.md/AGENTS.md) it coexists with.
//!
//! **Context vs. memory -- explicit, not incidental.** This module (plus
//! CLAUDE.md/AGENTS.md) answers *how an agent should work*: conventions,
//! process, style -- authored by a person, read fresh every session, and
//! never accumulated automatically. `super::memory` answers a different
//! question, *what a past session learned*: durable facts written by
//! `remember`/handoff-harvest and recalled by key. Neither is a substitute
//! for the other -- a `.zirv/context/common.md` rule does not belong in the
//! memory bank, and a memory fact (e.g. "the staging DB migration needs a
//! manual grant") does not belong in `common.md`. `optimize.rs`'s own N7
//! boundary (a memory key/body never reaches the judgment model) protects
//! the same split from the opposite direction: this layer's whole point is
//! to be read and analysed as instructions, memory's whole point is that it
//! is not.
//!
//! **Trust.** Like every repo-owned surface, `.zirv/context/*.md` is
//! untrusted content: `Layer::ContextCommon`/`ContextClaude`/`ContextCodex`
//! all map to `Scope::Repo`, so `Scope::trust` gives them `RepoUntrusted`,
//! the same as CLAUDE.md/AGENTS.md. It can steer a session's prose
//! instructions; it can never change what zirv itself runs or widen a
//! security setting -- see the test at the bottom of this module and
//! `REPO_FORBIDDEN` in `config.rs` for the same asymmetry enforced on
//! `ctx.toml`.

use std::path::{Path, PathBuf};

use super::optimize::{self, Layer, Surface};
use super::surface;

/// The subdirectory holding zirv's own canonical instruction layer.
pub const CONTEXT_DIR: &str = ".zirv/context";

/// `<repo>/.zirv/context/common.md` -- instructions common to every
/// Zirv-launched harness. Optional: a repo with none of this module's three
/// files analyses to nothing extra, exactly like a repo with no CLAUDE.md.
pub fn common_path(repo: &Path) -> PathBuf {
    repo.join(CONTEXT_DIR).join("common.md")
}

/// `<repo>/.zirv/context/claude.md` -- optional Claude-specific additions.
pub fn claude_path(repo: &Path) -> PathBuf {
    repo.join(CONTEXT_DIR).join("claude.md")
}

/// `<repo>/.zirv/context/codex.md` -- optional Codex-specific additions.
pub fn codex_path(repo: &Path) -> PathBuf {
    repo.join(CONTEXT_DIR).join("codex.md")
}

/// Where one `Instructions`-kind layer sits in the deterministic precedence
/// order the context compiler composes layers in: canonical common
/// content applies first, a harness-specific canonical addition layers on
/// top of it, and a harness's own native instruction file (CLAUDE.md /
/// AGENTS.md) composes last -- closest to the session. Within the native
/// tier, scope narrows the same way both harnesses' own documented override
/// order does: a nested file overrides its repo root, which overrides the
/// operator's global file (fix round 1, review finding 12-2 -- collapsing
/// every scope into one `Native` value hid this real, common shadowing case
/// entirely: a nested CLAUDE.md is *supposed* to win over the repo root one,
/// not tie with it). `PartialOrd`/`Ord` are derived from declaration order,
/// the same technique `Severity` (above) uses for its own ranking.
/// Consumed by `drift.rs`'s precedence/shadowing findings (issue #42);
/// Task 14's compiler is the second real consumer, for actual layer
/// ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrecedenceTier {
    CanonicalCommon,
    CanonicalHarnessSpecific,
    NativeGlobal,
    NativeRepo,
    NativeNested,
}

impl PrecedenceTier {
    pub fn label(&self) -> &'static str {
        match self {
            PrecedenceTier::CanonicalCommon => "canonical common",
            PrecedenceTier::CanonicalHarnessSpecific => "canonical harness-specific",
            PrecedenceTier::NativeGlobal => "native (global)",
            PrecedenceTier::NativeRepo => "native (repo)",
            PrecedenceTier::NativeNested => "native (nested)",
        }
    }
}

/// `None` for anything that is not an `Instructions`-kind layer (settings/
/// policy surfaces are a different dimension entirely -- see `Layer::kind`
/// and `Layer::is_settings`). Every `Instructions` layer has an opinion here;
/// `every_instructions_layer_has_a_defined_tier` (below) iterates
/// `optimize::ALL_LAYERS` rather than a hand-picked subset, so a future
/// `Instructions`-kind variant added here without a matching arm fails that
/// test instead of silently falling through the wildcard below.
pub fn precedence_tier(layer: Layer) -> Option<PrecedenceTier> {
    match layer {
        Layer::ContextCommon => Some(PrecedenceTier::CanonicalCommon),
        Layer::ContextClaude | Layer::ContextCodex => {
            Some(PrecedenceTier::CanonicalHarnessSpecific)
        }
        Layer::GlobalClaudeMd | Layer::GlobalAgentsMd | Layer::GlobalZirvMd => {
            Some(PrecedenceTier::NativeGlobal)
        }
        Layer::RepoClaudeMd | Layer::RepoAgentsMd | Layer::RepoZirvMd | Layer::RepoAgentMd => {
            Some(PrecedenceTier::NativeRepo)
        }
        Layer::NestedClaudeMd
        | Layer::NestedAgentsMd
        | Layer::NestedZirvMd
        | Layer::NestedAgentMd => Some(PrecedenceTier::NativeNested),
        _ => None,
    }
}

/// Same-directory precedence among the four native instruction-file kinds,
/// independent of `precedence_tier`'s global/repo/nested axis: `ZIRV.md`
/// outranks `AGENTS.md`, which outranks `CLAUDE.md`, which outranks the
/// singular `AGENT.md` compatibility alias (issue #538's file-and-precedence
/// contract). Only meaningful for a layer `is_same_directory_precedence_
/// candidate` accepts -- the value returned for any other layer (settings,
/// canonical `.zirv/context/`) is never read by `resolve_instruction_winners`.
pub fn within_tier_rank(layer: Layer) -> u8 {
    match layer {
        Layer::GlobalZirvMd | Layer::RepoZirvMd | Layer::NestedZirvMd => 0,
        Layer::GlobalAgentsMd | Layer::RepoAgentsMd | Layer::NestedAgentsMd => 1,
        Layer::GlobalClaudeMd | Layer::RepoClaudeMd | Layer::NestedClaudeMd => 2,
        Layer::RepoAgentMd | Layer::NestedAgentMd => 3,
        _ => u8::MAX,
    }
}

/// Whether `layer` ever competes for same-directory precedence at all. The
/// four native instruction-file kinds do; canonical `.zirv/context/` content
/// and every settings/policy surface do not -- there is only ever one of
/// each of those per repo, so there is nothing to shadow.
pub fn is_same_directory_precedence_candidate(layer: Layer) -> bool {
    within_tier_rank(layer) != u8::MAX
}

/// Whether `layer` is the singular `AGENT.md` compatibility alias -- the one
/// kind that always carries a migration diagnostic recommending rename to
/// `AGENTS.md`, regardless of whether it actually won its directory's
/// precedence (issue #538, file-and-precedence contract item 6).
fn is_singular_agent_md(layer: Layer) -> bool {
    matches!(layer, Layer::RepoAgentMd | Layer::NestedAgentMd)
}

/// A same-directory instruction file's resolution outcome, per issue #538's
/// dedup contract: `Included` is the one winner in its (scope, directory)
/// group; every other candidate in that group is `Shadowed` by the winner,
/// unless it is provably the same content (`Duplicate`) or could not be read
/// as trusted content at all (`Excluded`). Nothing collected is ever dropped
/// from a `Vec<Resolved>` -- every surface and every `optimize::Exclusion`
/// gets exactly one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Included,
    Shadowed {
        by: PathBuf,
    },
    Duplicate {
        of: PathBuf,
    },
    /// Review fix (issue #538): this file was shadowed by its directory's
    /// rank winner, but the WINNER's own content is a lone import of THIS
    /// file -- so this file's real text is what actually reaches the
    /// session (under the winner's own provenance, via `Resolved::
    /// delivers_from` on the winner's entry), not a blank shadow. `by`
    /// names the winner (the import stub that references it).
    IncludedByReference {
        by: PathBuf,
    },
    Excluded {
        reason: String,
    },
}

impl Decision {
    /// The `included | shadowed by <path> | duplicate of <path> | included
    /// by reference from <path> | excluded: <reason>` vocabulary `zirv
    /// context status` renders verbatim (issue #538, acceptance bullet 6:
    /// "context status shows ... included/shadowed/truncated reason").
    pub fn render(&self) -> String {
        match self {
            Decision::Included => "included".to_string(),
            Decision::Shadowed { by } => format!("shadowed by {}", by.display()),
            Decision::Duplicate { of } => format!("duplicate of {}", of.display()),
            Decision::IncludedByReference { by } => {
                format!("included by reference from {}", by.display())
            }
            Decision::Excluded { reason } => format!("excluded: {reason}"),
        }
    }
}

/// One surface's (or exclusion's) resolution, as `resolve_instruction_
/// winners` reports it. `path` is the join key back to the caller's own
/// `optimize::Surface`/`optimize::Exclusion` list -- there is no shared
/// numeric index between the two collections this function reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub layer: Layer,
    pub decision: Decision,
    /// Set only for a singular `AGENT.md` candidate: a diagnostic
    /// recommending rename to `AGENTS.md`, independent of `decision`.
    pub migration: Option<String>,
    /// Review fix (issue #538): `Some(target)` only on an `Included` winner
    /// whose own content is a lone import of another already-collected
    /// surface -- the DELIVERED text for this entry must be read from
    /// `target`, not from `path`, so a `ZIRV.md` reading `@AGENTS.md`
    /// actually delivers `AGENTS.md`'s real content instead of the eight
    /// literal bytes `@AGENTS.md`. `None` in every other case (deliver this
    /// entry's own text, the ordinary path).
    pub delivers_from: Option<PathBuf>,
}

fn scope_rank(scope: surface::Scope) -> u8 {
    match scope {
        surface::Scope::Global => 0,
        surface::Scope::Repo => 1,
        surface::Scope::Nested => 2,
        surface::Scope::LocalPrivate => 3,
    }
}

/// The directory a candidate's same-directory precedence groups by. Always
/// the file's own parent directory, with one normalization: `<repo>/.zirv/
/// ZIRV.md` groups with `<repo>/ZIRV.md` at the repo root (issue #538's file
/// contract, item 2 -- "or `<repo>/.zirv/ZIRV.md` when the root file is
/// absent"), since both name the same logical repo-scope source, not two
/// files in different directories that happen to share a filename.
fn logical_dir(path: &Path) -> PathBuf {
    let is_zirv_md = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("ZIRV.md"));
    let parent_is_dot_zirv = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(".zirv"));
    if is_zirv_md && parent_is_dot_zirv {
        path.parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    }
}

/// Whether `path` sits outside the trust root its own scope requires:
/// repo-owned layers (`Repo`/`Nested`/`LocalPrivate`) must stay inside
/// `repo`; a `Global` layer must stay inside `home` (or is treated as
/// escaping when `home` is unknown -- fail closed, the same posture
/// `surface::ContextSurface::for_path` holds for trust classification).
/// Consumed only by import-chain resolution below: an import target that
/// escapes its file's own trust root is refused, never followed.
fn escapes_trust_root(path: &Path, layer: Layer, repo: &Path, home: Option<&Path>) -> bool {
    let root = if layer.scope() == surface::Scope::Global {
        home
    } else {
        Some(repo)
    };
    match root {
        Some(root) => !path.starts_with(root),
        None => true,
    }
}

/// Path equality for import/dedup resolution, matching how the rest of the
/// collector already treats paths on Windows -- `logical_dir` above already
/// compares the `ZIRV.md`/`.zirv` filename segments with `eq_ignore_ascii_
/// case` for the same reason. Review fix (issue #538): without this, `@AGENTS.MD`
/// never matched an on-disk `AGENTS.md` on a case-insensitive filesystem,
/// silently falling through to `Shadowed` (or worse, `Excluded`) instead of
/// the dedup/import-expansion rules this module exists to apply.
fn paths_equal(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(b.to_string_lossy().as_ref())
    } else {
        a == b
    }
}

/// The single import/include target named by `text`, when `text`'s entire
/// non-blank content is exactly one recognized import line -- the dedup
/// contract's "whose entire non-blank content is a single import of the
/// winner" (issue #538, item 3). Supports the Claude Code `@file` syntax
/// (`@AGENTS.md`, `@./AGENTS.md`) and a lone markdown link or `include` line.
/// `None` for anything else (multiple non-blank lines, or a single line that
/// is not a recognized import) -- ordinary prose is never mistaken for an
/// import.
fn parse_lone_import(text: &str) -> Option<String> {
    let mut target: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if target.is_some() {
            return None;
        }
        target = Some(extract_import_target(line)?);
    }
    target
}

fn extract_import_target(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix('@') {
        let rest = rest.trim();
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if let Some(rest) = line.strip_prefix("include ") {
        let rest = rest.trim();
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if let Some(open) = line.strip_prefix('[') {
        let close = open.find("](")?;
        let after = &open[close + 2..];
        let target = after.strip_suffix(')')?;
        return (!target.is_empty()).then(|| target.to_string());
    }
    None
}

/// An import candidate's chase to a final target, bounded to `MAX_IMPORT_
/// DEPTH` hops and detecting cycles -- issue #538, item 3: "Bound include/
/// import depth, reject cycles and refuse escapes from the allowed trust
/// root." Resolved purely against the already-collected `surfaces` slice (no
/// filesystem access): an import target that is not itself one of the
/// already-collected sibling surfaces is treated as the chase's end point
/// (`Resolves`), not as a read failure -- this function never reads a file
/// resolve_instruction_winners's own caller has not already handed it.
enum ImportResolution {
    Resolves(PathBuf),
    Cycle,
    EscapesTrustRoot,
    /// Review fix (issue #538): a genuinely non-cyclic import chain deeper
    /// than `MAX_IMPORT_DEPTH` hops -- distinct from `Cycle` (which means a
    /// path was revisited), so the reported reason does not accuse a
    /// legitimate long chain of looping when it never did.
    DepthExceeded,
}

const MAX_IMPORT_DEPTH: usize = 3;

fn resolve_import_chain(
    start: &Surface,
    surfaces: &[Surface],
    repo: &Path,
    home: Option<&Path>,
) -> Option<ImportResolution> {
    let dir = start.path.parent()?;
    let target_str = parse_lone_import(&start.text)?;
    let mut current = optimize::normalize_lexically(dir, Path::new(&target_str));
    let mut visited: Vec<PathBuf> = vec![start.path.clone()];

    for _ in 0..MAX_IMPORT_DEPTH {
        if escapes_trust_root(&current, start.layer, repo, home) {
            return Some(ImportResolution::EscapesTrustRoot);
        }
        if visited.iter().any(|seen| paths_equal(seen, &current)) {
            return Some(ImportResolution::Cycle);
        }
        visited.push(current.clone());
        match surfaces.iter().find(|s| paths_equal(&s.path, &current)) {
            // Item 4 (nit): once matched, resolve to the SURFACE's own
            // canonical on-disk path, not the as-written import string's
            // normalized form -- `@AGENTS.MD` must deliver from the real
            // `/repo/AGENTS.md` a caller's own `surfaces` list (and every
            // later exact-path lookup against it) actually holds, not a
            // differently-cased path nothing downstream will match.
            Some(next) => match parse_lone_import(&next.text) {
                Some(next_target) => {
                    let Some(next_dir) = next.path.parent() else {
                        return Some(ImportResolution::Resolves(next.path.clone()));
                    };
                    current = optimize::normalize_lexically(next_dir, Path::new(&next_target));
                }
                None => return Some(ImportResolution::Resolves(next.path.clone())),
            },
            None => return Some(ImportResolution::Resolves(current)),
        }
    }
    // Depth exhausted without settling on a final target. Unlike `Cycle`
    // (a path was actually revisited), nothing here repeated -- this is a
    // real, if unusually long, import chain that simply exceeds the bound.
    Some(ImportResolution::DepthExceeded)
}

/// Resolves same-directory precedence among every collected instruction
/// surface and every refused candidate (`optimize::Exclusion`), per issue
/// #538's file-and-precedence contract: within a (scope, logical directory)
/// group, the lowest `within_tier_rank` wins (`Included`); every other
/// member is `Shadowed` by the winner unless it is provably the same content
/// or a lone import resolving to the winner (`Duplicate`), or could not be
/// trusted at all (`Excluded`). A surface outside every precedence group
/// (canonical `.zirv/context/`, settings) is always `Included` -- there is
/// nothing in its own directory competing with it.
///
/// Deliberately takes `repo`/`home` (unlike a fully filesystem-free pure
/// function) so import-chain resolution can enforce the trust-root-escape
/// rule; no filesystem access happens here -- every read this function needs
/// is already in `surfaces`/`exclusions`, both collected by the caller.
pub fn resolve_instruction_winners(
    surfaces: &[Surface],
    exclusions: &[optimize::Exclusion],
    repo: &Path,
    home: Option<&Path>,
) -> Vec<Resolved> {
    let mut result = Vec::new();
    let mut groups: std::collections::BTreeMap<(u8, PathBuf), Vec<usize>> =
        std::collections::BTreeMap::new();

    for (i, s) in surfaces.iter().enumerate() {
        if !is_same_directory_precedence_candidate(s.layer) {
            result.push(Resolved {
                path: s.path.clone(),
                layer: s.layer,
                decision: Decision::Included,
                migration: None,
                delivers_from: None,
            });
            continue;
        }
        let key = (scope_rank(s.layer.scope()), logical_dir(&s.path));
        groups.entry(key).or_default().push(i);
    }

    for (_, mut idxs) in groups {
        idxs.sort_by_key(|&i| (within_tier_rank(surfaces[i].layer), i));
        let winner_idx = idxs[0];
        let winner_path = surfaces[winner_idx].path.clone();
        let winner_text = surfaces[winner_idx].text.clone();
        for &i in &idxs {
            let s = &surfaces[i];
            let migration = is_singular_agent_md(s.layer)
                .then(|| format!("rename {} to AGENTS.md", s.path.display()));
            let decision = if i == winner_idx {
                Decision::Included
            } else if s.text == winner_text {
                Decision::Duplicate {
                    of: winner_path.clone(),
                }
            } else {
                match resolve_import_chain(s, surfaces, repo, home) {
                    Some(ImportResolution::Resolves(target))
                        if paths_equal(&target, &winner_path) =>
                    {
                        Decision::Duplicate {
                            of: winner_path.clone(),
                        }
                    }
                    Some(ImportResolution::Resolves(_)) | None => Decision::Shadowed {
                        by: winner_path.clone(),
                    },
                    Some(ImportResolution::Cycle) => Decision::Excluded {
                        reason: "import cycle".to_string(),
                    },
                    Some(ImportResolution::DepthExceeded) => Decision::Excluded {
                        reason: "import depth exceeded".to_string(),
                    },
                    Some(ImportResolution::EscapesTrustRoot) => Decision::Excluded {
                        reason: "escapes trust root".to_string(),
                    },
                }
            };
            result.push(Resolved {
                path: s.path.clone(),
                layer: s.layer,
                decision,
                migration,
                delivers_from: None,
            });
        }
    }

    for exclusion in exclusions {
        let migration = is_singular_agent_md(exclusion.layer)
            .then(|| format!("rename {} to AGENTS.md", exclusion.path.display()));
        // A symlinked candidate is still refused (never read as this file's
        // own content -- `reason` says so), but when its link target IS the
        // exact path of that directory's winner, it is the same content by
        // construction: `Duplicate`, not a blanket `Excluded` (issue #538,
        // item 3: "a shadowed file ... which is a symlink resolving to the
        // winner").
        let resolves_to_a_winner = exclusion.symlink_target.as_ref().is_some_and(|target| {
            result
                .iter()
                .any(|r| r.decision == Decision::Included && paths_equal(&r.path, target))
        });
        let decision = if resolves_to_a_winner {
            Decision::Duplicate {
                of: exclusion.symlink_target.clone().expect("checked above"),
            }
        } else {
            Decision::Excluded {
                reason: exclusion.reason.to_string(),
            }
        };
        result.push(Resolved {
            path: exclusion.path.clone(),
            layer: exclusion.layer,
            decision,
            migration,
            delivers_from: None,
        });
    }

    // Review fix (issue #538): a WINNER's own content can itself be a lone
    // import -- the file-and-precedence contract's compatibility-link
    // pattern (`ZIRV.md` containing exactly `@AGENTS.md`) is a winner doing
    // this, not a loser. Without this pass, that winner delivered its own
    // literal stub text and the file it names stayed `Shadowed`, silently
    // dropping the repository's real instructions. For every `Included`
    // entry whose own surface text is a lone import resolving (bounded,
    // cycle- and trust-root-checked, exactly like the loser-side chase
    // above) to another already-collected surface: record where the winner
    // must actually deliver its text FROM, and -- when the imported file is
    // itself one of THIS run's `Resolved` entries and is not already
    // `Included` -- promote it from `Shadowed`/`Duplicate`/`Excluded` to
    // `IncludedByReference`, naming the winner. A cycle or a trust-root
    // escape on the WINNER's own chase is left alone: the winner still
    // delivers its literal (un-expandable) text, exactly as before this fix,
    // since there is nothing safe to substitute it with.
    let mut deliveries: Vec<(usize, PathBuf)> = Vec::new();
    for (index, resolved) in result.iter().enumerate() {
        if resolved.decision != Decision::Included {
            continue;
        }
        let Some(surface) = surfaces
            .iter()
            .find(|s| paths_equal(&s.path, &resolved.path))
        else {
            continue;
        };
        if let Some(ImportResolution::Resolves(target)) =
            resolve_import_chain(surface, surfaces, repo, home)
            && !paths_equal(&target, &surface.path)
            && surfaces.iter().any(|s| paths_equal(&s.path, &target))
        {
            deliveries.push((index, target));
        }
    }
    for (index, target) in deliveries {
        result[index].delivers_from = Some(target.clone());
        let winner_path = result[index].path.clone();
        if let Some(referenced) = result
            .iter_mut()
            .find(|r| paths_equal(&r.path, &target) && r.decision != Decision::Included)
        {
            referenced.decision = Decision::IncludedByReference { by: winner_path };
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::optimize::ALL_LAYERS;
    use crate::commands::ctx::surface::{Kind, Trust};

    #[test]
    fn precedence_ranks_canonical_common_below_harness_specific_below_native() {
        assert!(PrecedenceTier::CanonicalCommon < PrecedenceTier::CanonicalHarnessSpecific);
        assert!(PrecedenceTier::CanonicalHarnessSpecific < PrecedenceTier::NativeGlobal);
    }

    /// Within the native tier, the more specific scope wins: nested
    /// overrides repo, repo overrides global -- the real shadowing case a
    /// single collapsed `Native` value hid (fix round 1, review finding
    /// 12-2).
    #[test]
    fn native_precedence_narrows_global_below_repo_below_nested() {
        assert!(PrecedenceTier::NativeGlobal < PrecedenceTier::NativeRepo);
        assert!(PrecedenceTier::NativeRepo < PrecedenceTier::NativeNested);
    }

    /// Iterates every `Layer` variant (`optimize::ALL_LAYERS`) rather than a
    /// hand-picked subset, so a future `Instructions`-kind variant added
    /// without a matching `precedence_tier` arm fails this test instead of
    /// silently falling through that function's wildcard arm (fix round 1,
    /// review finding 12-2).
    #[test]
    fn every_instructions_layer_has_a_defined_tier() {
        for layer in ALL_LAYERS.iter().copied() {
            if layer.kind() == Kind::Instructions {
                assert!(precedence_tier(layer).is_some(), "{layer:?} has no tier");
            }
        }
    }

    #[test]
    fn settings_layers_have_no_precedence_tier() {
        for layer in [
            Layer::UserSettings,
            Layer::ProjectSettings,
            Layer::LocalSettings,
            Layer::CodexUserSettings,
            Layer::CodexProjectSettings,
        ] {
            assert_eq!(precedence_tier(layer), None, "{layer:?}");
        }
    }

    #[test]
    fn path_helpers_join_the_repo_root() {
        let repo = Path::new("/repo");
        assert_eq!(common_path(repo), repo.join(".zirv/context/common.md"));
        assert_eq!(claude_path(repo), repo.join(".zirv/context/claude.md"));
        assert_eq!(codex_path(repo), repo.join(".zirv/context/codex.md"));
    }

    /// Issue #41's binding requirement: repo-owned canonical context can
    /// never grant operator authority, no matter what its content says.
    /// Proven the same way Task 9/10 proved it for CLAUDE.md/AGENTS.md --
    /// every new `Layer` variant's `scope()` is `Repo`, and `Scope::trust`
    /// has no path from `Repo` to `Trust::Operator`.
    #[test]
    fn repo_owned_canonical_context_can_never_grant_operator_permissions() {
        for layer in [
            Layer::ContextCommon,
            Layer::ContextClaude,
            Layer::ContextCodex,
        ] {
            assert_eq!(
                layer.trust(),
                Trust::RepoUntrusted,
                "{layer:?} must never be operator-trusted, no matter its content"
            );
            assert!(layer.is_repo_owned());
        }
    }

    fn surface(layer: Layer, path: PathBuf, text: &str) -> Surface {
        Surface {
            layer,
            path,
            text: text.to_string(),
        }
    }

    /// Issue #538, file contract item 2: when both `<repo>/ZIRV.md` and
    /// `<repo>/.zirv/ZIRV.md` exist, the root file always wins and the
    /// `.zirv/` fallback is reported shadowed by it -- never the reverse.
    #[test]
    fn dot_zirv_zirv_md_is_shadowed_by_the_root_file() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(Layer::RepoZirvMd, repo.join("ZIRV.md"), "root"),
            surface(
                Layer::RepoZirvMd,
                repo.join(".zirv").join("ZIRV.md"),
                "fallback",
            ),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);

        let root = resolved
            .iter()
            .find(|r| r.path == repo.join("ZIRV.md"))
            .expect("root entry present");
        assert_eq!(root.decision, Decision::Included);

        let fallback = resolved
            .iter()
            .find(|r| r.path == repo.join(".zirv").join("ZIRV.md"))
            .expect("fallback entry present");
        assert_eq!(
            fallback.decision,
            Decision::Shadowed {
                by: repo.join("ZIRV.md")
            }
        );
    }

    /// Issue #538, file contract: at the same directory, `ZIRV.md` outranks
    /// `AGENTS.md`, which outranks `CLAUDE.md`, which outranks the singular
    /// `AGENT.md` compatibility alias.
    #[test]
    fn same_directory_precedence_is_zirv_then_agents_then_claude_then_agent() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(Layer::RepoAgentMd, repo.join("AGENT.md"), "agent"),
            surface(Layer::RepoClaudeMd, repo.join("CLAUDE.md"), "claude"),
            surface(Layer::RepoAgentsMd, repo.join("AGENTS.md"), "agents"),
            surface(Layer::RepoZirvMd, repo.join("ZIRV.md"), "zirv"),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);
        let decision_for = |name: &str| -> Decision {
            resolved
                .iter()
                .find(|r| r.path.ends_with(name))
                .unwrap_or_else(|| panic!("{name} present"))
                .decision
                .clone()
        };

        assert_eq!(decision_for("ZIRV.md"), Decision::Included);
        let by = repo.join("ZIRV.md");
        assert_eq!(
            decision_for("AGENTS.md"),
            Decision::Shadowed { by: by.clone() }
        );
        assert_eq!(
            decision_for("CLAUDE.md"),
            Decision::Shadowed { by: by.clone() }
        );
        assert_eq!(decision_for("AGENT.md"), Decision::Shadowed { by });
    }

    /// Issue #538, file contract item 6: `AGENT.md` is a real candidate
    /// (wins its directory) only when no `AGENTS.md` sibling exists; either
    /// way it always carries the migration diagnostic.
    #[test]
    fn singular_agent_md_is_a_candidate_only_without_agents_md_and_warns() {
        let repo = Path::new("/repo");

        let alone = vec![surface(Layer::RepoAgentMd, repo.join("AGENT.md"), "agent")];
        let resolved = resolve_instruction_winners(&alone, &[], repo, None);
        let entry = &resolved[0];
        assert_eq!(entry.decision, Decision::Included);
        assert!(
            entry
                .migration
                .as_deref()
                .is_some_and(|m| m.contains("AGENTS.md")),
            "{entry:?}"
        );

        let with_agents = vec![
            surface(Layer::RepoAgentMd, repo.join("AGENT.md"), "agent"),
            surface(Layer::RepoAgentsMd, repo.join("AGENTS.md"), "agents"),
        ];
        let resolved = resolve_instruction_winners(&with_agents, &[], repo, None);
        let agent = resolved
            .iter()
            .find(|r| r.path.ends_with("AGENT.md"))
            .expect("AGENT.md entry present");
        assert_eq!(
            agent.decision,
            Decision::Shadowed {
                by: repo.join("AGENTS.md")
            },
            "AGENT.md is not a usable candidate once AGENTS.md exists"
        );
        assert!(
            agent.migration.is_some(),
            "the migration diagnostic is unconditional"
        );
    }

    /// Issue #538, item 3: identical content is a `Duplicate`, not a
    /// `Shadowed` -- "the same rules consume context once".
    #[test]
    fn an_identical_compatibility_file_is_reported_as_a_duplicate_not_shadowed() {
        let repo = Path::new("/repo");
        let text = "- always run the full test suite\n";
        let surfaces = vec![
            surface(Layer::RepoAgentsMd, repo.join("AGENTS.md"), text),
            surface(Layer::RepoClaudeMd, repo.join("CLAUDE.md"), text),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);
        let claude = resolved
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .expect("CLAUDE.md entry present");
        assert_eq!(
            claude.decision,
            Decision::Duplicate {
                of: repo.join("AGENTS.md")
            }
        );
    }

    /// Issue #538, item 3: a `CLAUDE.md` whose entire content is a single
    /// `@AGENTS.md` import is a `Duplicate` of the winner it imports, not a
    /// plain `Shadowed` -- "the same rules consume context once".
    #[test]
    fn a_claude_md_that_only_imports_agents_md_is_consumed_once() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(
                Layer::RepoAgentsMd,
                repo.join("AGENTS.md"),
                "- real rules\n",
            ),
            surface(Layer::RepoClaudeMd, repo.join("CLAUDE.md"), "@AGENTS.md\n"),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);
        let claude = resolved
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .expect("CLAUDE.md entry present");
        assert_eq!(
            claude.decision,
            Decision::Duplicate {
                of: repo.join("AGENTS.md")
            }
        );
    }

    /// Issue #538, item 3: a genuine import cycle, and an import target that
    /// escapes the trust root, both fail safe as `Excluded` with a named
    /// reason -- never silently followed or silently dropped.
    #[test]
    fn import_cycles_and_trust_root_escapes_are_excluded_with_a_reason() {
        let repo = Path::new("/repo");

        // CLAUDE.md wins its directory (rank 2 beats AGENT.md's rank 3); the
        // shadowed AGENT.md's own import chases back through CLAUDE.md's
        // import of AGENT.md, closing a cycle.
        let cyclic = vec![
            surface(Layer::RepoClaudeMd, repo.join("CLAUDE.md"), "@AGENT.md\n"),
            surface(Layer::RepoAgentMd, repo.join("AGENT.md"), "@CLAUDE.md\n"),
        ];
        let resolved = resolve_instruction_winners(&cyclic, &[], repo, None);
        let claude = resolved
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .expect("CLAUDE.md entry present");
        assert_eq!(claude.decision, Decision::Included);
        let agent = resolved
            .iter()
            .find(|r| r.path.ends_with("AGENT.md"))
            .expect("AGENT.md entry present");
        assert!(
            matches!(&agent.decision, Decision::Excluded { reason } if reason == "import cycle"),
            "{:?}",
            agent.decision
        );

        // CLAUDE.md imports a path outside the repo checkout.
        let escaping = vec![
            surface(
                Layer::RepoAgentsMd,
                repo.join("AGENTS.md"),
                "- real rules\n",
            ),
            surface(
                Layer::RepoClaudeMd,
                repo.join("CLAUDE.md"),
                "@../outside/secrets.md\n",
            ),
        ];
        let resolved = resolve_instruction_winners(&escaping, &[], repo, None);
        let claude = resolved
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .expect("CLAUDE.md entry present");
        assert!(
            matches!(&claude.decision, Decision::Excluded { reason } if reason == "escapes trust root"),
            "{:?}",
            claude.decision
        );
    }

    /// A surface outside every same-directory precedence group (canonical
    /// `.zirv/context/`, settings) is always `Included` -- there is nothing
    /// in its own directory competing with it.
    #[test]
    fn a_non_candidate_surface_is_always_included() {
        let repo = Path::new("/repo");
        let surfaces = vec![surface(
            Layer::ContextCommon,
            repo.join(".zirv/context/common.md"),
            "- a rule\n",
        )];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);
        assert_eq!(resolved[0].decision, Decision::Included);
        assert_eq!(resolved[0].migration, None);
    }

    /// Issue #538, item 3 / the resolver-side half of "record the exclusion
    /// instead of skipping silently": a symlinked candidate is always
    /// reported -- `Duplicate` when its link target is exactly its
    /// directory's winner, `Excluded` with `reason` otherwise.
    #[test]
    fn a_symlinked_candidate_resolving_to_the_winner_is_a_duplicate_others_are_excluded() {
        let repo = Path::new("/repo");
        let surfaces = vec![surface(
            Layer::RepoAgentsMd,
            repo.join("AGENTS.md"),
            "rules",
        )];
        let exclusions = vec![
            optimize::Exclusion {
                layer: Layer::RepoClaudeMd,
                path: repo.join("CLAUDE.md"),
                reason: "symlinked instruction file",
                symlink_target: Some(repo.join("AGENTS.md")),
            },
            optimize::Exclusion {
                layer: Layer::RepoZirvMd,
                path: repo.join("ZIRV.md"),
                reason: "symlinked instruction file",
                symlink_target: Some(PathBuf::from("/somewhere/else.md")),
            },
        ];
        let resolved = resolve_instruction_winners(&surfaces, &exclusions, repo, None);

        let claude = resolved
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .expect("CLAUDE.md exclusion present");
        assert_eq!(
            claude.decision,
            Decision::Duplicate {
                of: repo.join("AGENTS.md")
            }
        );

        let zirv = resolved
            .iter()
            .find(|r| r.path.ends_with("ZIRV.md"))
            .expect("ZIRV.md exclusion present");
        assert!(
            matches!(&zirv.decision, Decision::Excluded { reason } if reason == "symlinked instruction file"),
            "{:?}",
            zirv.decision
        );
    }

    // -- review fix (issue #538): winner-side import expansion --------------

    /// Item 1 (blocker): the WINNER's own content can be a lone import too --
    /// exactly the compatibility-link pattern `--report`/the README tell
    /// users to create (`ZIRV.md` containing only `@AGENTS.md`). The
    /// winner's literal stub text must never be delivered as-is: the
    /// imported file's real content is what the winner actually delivers
    /// (`delivers_from`), and the imported file's own entry is `Included
    /// ByReference`, not a blank `Shadowed` that would silently drop the
    /// repository's real instructions.
    #[test]
    fn a_winner_that_only_imports_the_loser_delivers_the_losers_real_text() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(Layer::RepoZirvMd, repo.join("ZIRV.md"), "@AGENTS.md\n"),
            surface(
                Layer::RepoAgentsMd,
                repo.join("AGENTS.md"),
                "- the real rules live here\n",
            ),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);

        let winner = resolved
            .iter()
            .find(|r| r.path.ends_with("ZIRV.md"))
            .expect("ZIRV.md entry present");
        assert_eq!(winner.decision, Decision::Included);
        assert_eq!(
            winner.delivers_from,
            Some(repo.join("AGENTS.md")),
            "the winner must deliver AGENTS.md's real text, not its own @AGENTS.md stub"
        );

        let imported = resolved
            .iter()
            .find(|r| r.path.ends_with("AGENTS.md"))
            .expect("AGENTS.md entry present");
        assert_eq!(
            imported.decision,
            Decision::IncludedByReference {
                by: repo.join("ZIRV.md")
            },
            "the imported file must be reported as reached, not merely shadowed"
        );
    }

    /// Item 1 (blocker), the other direction: a LOSER that only imports the
    /// winner is `Duplicate`, unaffected by the winner-side fix above --
    /// `a_claude_md_that_only_imports_agents_md_is_consumed_once` already
    /// pins this; this test names it explicitly as "both directions" the
    /// review asked for, using a `ZIRV.md` winner instead of `AGENTS.md`, so
    /// the two tests are not accidentally exercising the same code path.
    #[test]
    fn a_loser_that_only_imports_the_winner_is_a_duplicate_not_included_by_reference() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(
                Layer::RepoZirvMd,
                repo.join("ZIRV.md"),
                "- the real rules\n",
            ),
            surface(Layer::RepoAgentsMd, repo.join("AGENTS.md"), "@ZIRV.md\n"),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);

        let winner = resolved
            .iter()
            .find(|r| r.path.ends_with("ZIRV.md"))
            .expect("ZIRV.md entry present");
        assert_eq!(winner.decision, Decision::Included);
        assert_eq!(
            winner.delivers_from, None,
            "the winner does not import anything itself"
        );

        let loser = resolved
            .iter()
            .find(|r| r.path.ends_with("AGENTS.md"))
            .expect("AGENTS.md entry present");
        assert_eq!(
            loser.decision,
            Decision::Duplicate {
                of: repo.join("ZIRV.md")
            }
        );
    }

    /// Item 3 (should): a genuinely non-cyclic import chain deeper than
    /// `MAX_IMPORT_DEPTH` gets its own reason, distinct from "import
    /// cycle" -- nothing in this chain was ever revisited.
    #[test]
    fn an_import_chain_deeper_than_the_limit_is_excluded_as_depth_exceeded_not_a_cycle() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(
                Layer::RepoAgentsMd,
                repo.join("AGENTS.md"),
                "- the winner\n",
            ),
            surface(Layer::RepoClaudeMd, repo.join("CLAUDE.md"), "@a.md\n"),
            surface(Layer::ContextCommon, repo.join("a.md"), "@b.md\n"),
            surface(Layer::ContextCommon, repo.join("b.md"), "@c.md\n"),
            surface(Layer::ContextCommon, repo.join("c.md"), "@d.md\n"),
            surface(Layer::ContextCommon, repo.join("d.md"), "- real content\n"),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);

        let claude = resolved
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .expect("CLAUDE.md entry present");
        assert!(
            matches!(&claude.decision, Decision::Excluded { reason } if reason == "import depth exceeded"),
            "a long-but-non-cyclic chain must not be misreported as a cycle: {:?}",
            claude.decision
        );
    }

    /// Item 4 (nit): import/dedup path comparisons must match how the rest
    /// of the collector already treats paths on Windows.
    #[test]
    fn paths_equal_is_case_insensitive_on_windows_and_exact_elsewhere() {
        let a = Path::new("/repo/AGENTS.md");
        let b = Path::new("/repo/agents.md");
        assert!(paths_equal(a, a));
        if cfg!(windows) {
            assert!(
                paths_equal(a, b),
                "Windows paths compare case-insensitively"
            );
        } else {
            assert!(!paths_equal(a, b), "non-Windows paths compare exactly");
        }
    }

    /// Item 4 (nit), end to end: `@AGENTS.MD` (different case) must still
    /// resolve to an on-disk `AGENTS.md` on a case-insensitive filesystem.
    #[test]
    fn an_import_with_different_case_resolves_on_a_case_insensitive_filesystem() {
        let repo = Path::new("/repo");
        let surfaces = vec![
            surface(Layer::RepoZirvMd, repo.join("ZIRV.md"), "@AGENTS.MD\n"),
            surface(
                Layer::RepoAgentsMd,
                repo.join("AGENTS.md"),
                "- the real rules\n",
            ),
        ];
        let resolved = resolve_instruction_winners(&surfaces, &[], repo, None);
        let winner = resolved
            .iter()
            .find(|r| r.path.ends_with("ZIRV.md"))
            .expect("ZIRV.md entry present");
        if cfg!(windows) {
            assert_eq!(
                winner.delivers_from,
                Some(repo.join("AGENTS.md")),
                "Windows: @AGENTS.MD must resolve to the on-disk AGENTS.md"
            );
        } else {
            assert_eq!(
                winner.delivers_from, None,
                "a case-sensitive filesystem must not match a differently-cased import"
            );
        }
    }
}
