//! The team compiler (issue #541 chunk B): a deterministic mapping from an
//! execution profile to a bounded, inspectable [`TeamPlan`] -- which
//! concrete manifests staff the request, what each owns, how they depend on
//! one another, and why an obvious alternative was left out.
//!
//! [`compile`] is pure scheduler data, not model-written narration: given the
//! same objective, profile, agent registry and route eligibility, it always
//! returns the same plan. A bounded structured model decision to break a
//! genuine tie among otherwise-equal candidates is deliberately NOT
//! implemented in this chunk (see the design note's "deferred" section) --
//! every rule here is a plain deterministic function of its inputs.
//!
//! This module also owns the wrapped-harness CLI surface
//! (`zirv workflow team plan|show|brief`): the same compiler a native
//! coordinator will call directly in chunk C.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use super::agents::{AgentRegistry, ModelTier, team_role_for};
use super::capability::CapabilityId;
use super::classify::{self, Classification, Complexity, Intent, RiskBand};
use super::engine;
use super::profile::{ExecutionMode, ExecutionProfile, WorkDomainTag};
use super::skill::SkillRegistry;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::jev::{self, AdvisoryStatus, JevEffect, Question};
use crate::commands::ctx::state::StateDir;
use crate::commands::ctx::state::now_secs;
use crate::commands::ctx::team::{Authority, TeamRole};

pub const TEAM_PLAN_SCHEMA_VERSION: u32 = 1;

/// The exclusive resource boundary a writer seat owns, or the empty claim an
/// independent (never-shares-a-writer's-workspace) seat gets instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub paths: Vec<String>,
    pub worktree: bool,
}

impl Claim {
    fn none() -> Self {
        Self {
            paths: Vec::new(),
            worktree: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seat {
    pub id: String,
    pub manifest_id: String,
    pub manifest_version: u32,
    pub team_role: TeamRole,
    pub task: String,
    pub deliverable: String,
    pub required_capabilities: Vec<CapabilityId>,
    pub optional_capabilities: Vec<CapabilityId>,
    pub authority: Authority,
    pub claim: Claim,
    pub depends_on: Vec<String>,
    pub reason: String,
    pub result_schema: String,
    pub consumer: String,
    pub route_tier: ModelTier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelGroup {
    pub seat_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpendClass {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLimits {
    pub max_fan_out: usize,
    pub max_depth: usize,
    pub max_retries: usize,
    pub spend_class: SpendClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omission {
    pub manifest_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPlan {
    pub schema_version: u32,
    pub objective: String,
    pub profile: ExecutionProfile,
    pub seats: Vec<Seat>,
    pub groups: Vec<ParallelGroup>,
    pub limits: PlanLimits,
    pub omitted: Vec<Omission>,
    pub degraded: Vec<String>,
    pub created_at: u64,
}

impl TeamPlan {
    /// Every seat id `seat_id` transitively depends on, issue #541 chunk C
    /// review finding: an ancestor in the dependency graph is a HAND-OFF
    /// the plan already accounts for, never a claim conflict, however wide
    /// its own claim is -- `delegation::delegate`'s claim-overlap check
    /// excludes exactly this set. `seat_id` naming no seat in this plan (or
    /// a seat with no `depends_on`) returns an empty set -- never an error,
    /// since the caller already knows whether a seat matched before asking.
    pub fn ancestors_of(&self, seat_id: &str) -> std::collections::BTreeSet<&str> {
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut stack: Vec<&str> = self
            .seats
            .iter()
            .find(|seat| seat.id == seat_id)
            .map(|seat| seat.depends_on.iter().map(String::as_str).collect())
            .unwrap_or_default();
        while let Some(id) = stack.pop() {
            if seen.insert(id)
                && let Some(seat) = self.seats.iter().find(|seat| seat.id == id)
            {
                stack.extend(seat.depends_on.iter().map(String::as_str));
            }
        }
        seen
    }
}

fn limits_for(execution: ExecutionMode) -> (usize, usize, SpendClass) {
    match execution {
        ExecutionMode::Direct => (0, 0, SpendClass::Low),
        ExecutionMode::Bounded => (2, 1, SpendClass::Medium),
        ExecutionMode::Orchestrated => (6, 2, SpendClass::High),
    }
}

/// Maps a built-in manifest id to the result schema (`ctx::result_schema`'s
/// named schemas, or one of the free-standing `"findings"`/`"patch"`/
/// `"report"` shapes) its seat's evidence is expected to satisfy.
fn result_schema_for(manifest_id: &str) -> &'static str {
    match manifest_id {
        "reviewer" | "security-scanner" => "review",
        "implementer" | "doc-keeper" | "devops-sre" => "implement",
        "tester" => "test",
        "researcher" | "explorer" | "data-analyst" => "research",
        "planner" | "architect" => "report",
        "debugger" => "findings",
        _ => "report",
    }
}

/// What domain specialist a tag adds, beyond the base team, and whether that
/// specialist is a writer (owns its own claim, and every later independent
/// seat waits on it) or a read-only producer of findings.
fn specialist_for(tag: WorkDomainTag) -> Option<(&'static str, &'static str, &'static str, bool)> {
    match tag {
        WorkDomainTag::Data => Some((
            "data-analyst",
            "Provide reproducible data/query analysis for",
            "reproducible query evidence and findings",
            false,
        )),
        WorkDomainTag::Docs => Some((
            "doc-keeper",
            "Update documentation for",
            "synchronized documentation + verification report",
            true,
        )),
        WorkDomainTag::DevOps => Some((
            "devops-sre",
            "Own the CI/CD, infrastructure, or deployment change for",
            "changed operational paths + verification evidence",
            true,
        )),
        // Security is covered by `ValidationProfile::security_review`
        // (`profile::ExecutionProfile::derive` already sets it whenever the
        // Security domain tag is present), so it never needs a second,
        // duplicate specialist-selection path here. Frontend and
        // Architecture have no distinct manifest of their own: Frontend
        // work is implemented by the ordinary `implementer` seat(s), and
        // Architecture is handled by the `architect` seat added below.
        WorkDomainTag::Security
        | WorkDomainTag::Frontend
        | WorkDomainTag::Architecture
        | WorkDomainTag::General => None,
    }
}

/// One desired seat before it is resolved against a concrete registry and
/// route eligibility.
struct SeatSpec {
    manifest_id: String,
    task: String,
    deliverable: String,
    reason: String,
    depends_on: Vec<String>,
    claim: Claim,
    consumer: String,
}

/// Resolves one [`SeatSpec`] into a concrete [`Seat`], or an [`Omission`]
/// naming exactly why it could not be staffed -- an unknown manifest id, an
/// unresolvable skill reference, or a team role with no eligible route.
/// Never invents a manifest, capability, or route: every branch either
/// returns data read straight from `registry`/`skills`/`route_eligibility`,
/// or the omission that says so.
fn resolve_seat(
    id: String,
    spec: SeatSpec,
    registry: &AgentRegistry,
    skills: &SkillRegistry,
    route_eligibility: &dyn Fn(TeamRole) -> Result<(), String>,
) -> Result<Seat, Omission> {
    let agent = registry.get(&spec.manifest_id).map_err(|error| Omission {
        manifest_id: spec.manifest_id.clone(),
        reason: format!("unknown manifest: {error}"),
    })?;
    for skill_ref in &agent.manifest.skills {
        if let Err(error) = skills.get(&skill_ref.id) {
            return Err(Omission {
                manifest_id: spec.manifest_id.clone(),
                reason: format!("attached skill unresolvable: {error}"),
            });
        }
    }
    let team_role = team_role_for(&agent.manifest);
    if let Err(reason) = route_eligibility(team_role) {
        return Err(Omission {
            manifest_id: spec.manifest_id.clone(),
            reason: format!("no eligible route for team role '{team_role}': {reason}"),
        });
    }
    Ok(Seat {
        id,
        manifest_id: agent.manifest.id.clone(),
        manifest_version: agent.manifest.version,
        team_role,
        task: spec.task,
        deliverable: spec.deliverable,
        required_capabilities: agent.manifest.required_capabilities.clone(),
        optional_capabilities: agent.manifest.optional_capabilities.clone(),
        authority: team_role.authority(),
        claim: spec.claim,
        depends_on: spec.depends_on,
        reason: spec.reason,
        result_schema: result_schema_for(&spec.manifest_id).to_string(),
        consumer: spec.consumer,
        route_tier: agent.manifest.model_tier,
    })
}

fn try_add(
    seats: &mut Vec<Seat>,
    omitted: &mut Vec<Omission>,
    result: Result<Seat, Omission>,
) -> Option<String> {
    match result {
        Ok(seat) => {
            let id = seat.id.clone();
            seats.push(seat);
            Some(id)
        }
        Err(omission) => {
            omitted.push(omission);
            None
        }
    }
}

/// One implementer seat per claim boundary, capped by `max_fan_out`. The
/// FALLBACK count when [`Classification::changed_paths`] is empty (older
/// durable state, or a classification measured with no path list at all):
/// four files per claim group, as an approximation of a real path split.
fn implementer_seat_count(classification: &Classification, max_fan_out: usize) -> usize {
    let files = classification.changed_files.max(1);
    files.div_ceil(4).clamp(1, max_fan_out.max(1))
}

/// Groups `paths` into claim boundaries by TOP-LEVEL path component
/// (`src/foo/x.rs` and `src/foo/y.rs` share a claim group; `docs/x.md` gets
/// its own), merging the smallest groups together until the result is no
/// more than `max_fan_out` groups -- a repository with many top-level
/// directories still gets a BOUNDED number of implementer seats rather than
/// one per directory. Empty in, empty out: an empty `paths` means "no real
/// path list to split by", which [`claim_groups_for`] falls back from to
/// the count-based bucket.
fn claim_groups_from_paths(paths: &[String], max_fan_out: usize) -> Vec<Vec<String>> {
    if paths.is_empty() || max_fan_out == 0 {
        return Vec::new();
    }
    let mut by_root: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths {
        let root = path.split('/').next().unwrap_or(path.as_str()).to_string();
        by_root.entry(root).or_default().push(path.clone());
    }
    let mut groups: Vec<Vec<String>> = by_root.into_values().collect();
    groups.sort_by_key(|group| group.len());
    while groups.len() > max_fan_out {
        let smallest = groups.remove(0);
        let mut into = groups.remove(0);
        into.extend(smallest);
        groups.push(into);
        groups.sort_by_key(|group| group.len());
    }
    groups
}

/// The claim groups this Orchestrated implementer split actually uses: real
/// path-boundary groups from [`Classification::changed_paths`] when the
/// classification carries them, else the count-based bucket
/// [`implementer_seat_count`] always could (issue #541 chunk C, decision 3
/// -- resolves chunk B's "deferred: real per-path claim splitting").
fn claim_groups_for(classification: &Classification, max_fan_out: usize) -> Vec<Vec<String>> {
    let real = claim_groups_from_paths(&classification.changed_paths, max_fan_out);
    if !real.is_empty() {
        return real;
    }
    let count = implementer_seat_count(classification, max_fan_out);
    (1..=count).map(|n| vec![format!("group-{n}")]).collect()
}

/// Dependency-respecting parallel groups: seats with no unresolved
/// dependency go in group 0, seats depending only on group-0 seats go in
/// group 1, and so on. Assumes `seats` is already in an order where every
/// dependency of a seat was pushed before that seat -- true for every path
/// through [`compile`].
fn topological_groups(seats: &[Seat]) -> Vec<ParallelGroup> {
    if seats.is_empty() {
        return Vec::new();
    }
    let mut level: BTreeMap<String, usize> = BTreeMap::new();
    for seat in seats {
        let lvl = seat
            .depends_on
            .iter()
            .filter_map(|dependency| level.get(dependency))
            .max()
            .map_or(0, |max| max + 1);
        level.insert(seat.id.clone(), lvl);
    }
    let max_level = level.values().copied().max().unwrap_or(0);
    (0..=max_level)
        .map(|lvl| ParallelGroup {
            seat_ids: seats
                .iter()
                .filter(|seat| level.get(&seat.id) == Some(&lvl))
                .map(|seat| seat.id.clone())
                .collect(),
        })
        .collect()
}

/// Compiles the smallest capable team for `objective` under `profile`.
/// Pure and deterministic: identical inputs always compile to an identical
/// plan. See the module doc for what this chunk deliberately leaves out.
pub fn compile(
    objective: &str,
    profile: &ExecutionProfile,
    registry: &AgentRegistry,
    skills: &SkillRegistry,
    route_eligibility: &dyn Fn(TeamRole) -> Result<(), String>,
) -> CtxResult<TeamPlan> {
    let classification = &profile.classification;
    let (max_fan_out, max_depth, spend_class) = limits_for(profile.execution);
    let limits = PlanLimits {
        max_fan_out,
        max_depth,
        max_retries: 2,
        spend_class,
    };

    let mut seats: Vec<Seat> = Vec::new();
    let mut omitted: Vec<Omission> = Vec::new();
    let degraded: Vec<String> = Vec::new();

    if profile.execution != ExecutionMode::Direct {
        let mut writer_ids: Vec<String> = Vec::new();

        // 1. The primary writer(s), decided by intent and execution mode.
        if classification.intent == Intent::Bugfix {
            let debugger_id = try_add(
                &mut seats,
                &mut omitted,
                resolve_seat(
                    "debugger-1".to_string(),
                    SeatSpec {
                        manifest_id: "debugger".to_string(),
                        task: format!("Reproduce and root-cause: {objective}"),
                        deliverable: "failing reproduction test + root-cause note".to_string(),
                        reason: "bug fix needs reproduction and root cause before a patch"
                            .to_string(),
                        depends_on: Vec::new(),
                        claim: Claim {
                            paths: vec!["primary".to_string()],
                            worktree: false,
                        },
                        consumer: "implementer-1".to_string(),
                    },
                    registry,
                    skills,
                    route_eligibility,
                ),
            );
            if let Some(id) = &debugger_id {
                writer_ids.push(id.clone());
            }
            let implementer_id = try_add(
                &mut seats,
                &mut omitted,
                resolve_seat(
                    "implementer-1".to_string(),
                    SeatSpec {
                        manifest_id: "implementer".to_string(),
                        task: format!("Apply the fix for: {objective}"),
                        deliverable: "patch resolving the root cause + passing repro test"
                            .to_string(),
                        reason: "the fix itself, scoped by the debugger's root cause".to_string(),
                        depends_on: debugger_id.into_iter().collect(),
                        claim: Claim {
                            paths: vec!["primary".to_string()],
                            worktree: false,
                        },
                        consumer: "coordinator".to_string(),
                    },
                    registry,
                    skills,
                    route_eligibility,
                ),
            );
            if let Some(id) = implementer_id {
                writer_ids.push(id);
            }
        } else {
            match profile.execution {
                ExecutionMode::Bounded => {
                    let implementer_id = try_add(
                        &mut seats,
                        &mut omitted,
                        resolve_seat(
                            "implementer-1".to_string(),
                            SeatSpec {
                                manifest_id: "implementer".to_string(),
                                task: format!("Implement: {objective}"),
                                deliverable: "changed paths + fresh verification evidence"
                                    .to_string(),
                                reason: "bounded work needs one owning implementer".to_string(),
                                depends_on: Vec::new(),
                                claim: Claim {
                                    paths: vec!["primary".to_string()],
                                    worktree: false,
                                },
                                consumer: "coordinator".to_string(),
                            },
                            registry,
                            skills,
                            route_eligibility,
                        ),
                    );
                    if let Some(id) = implementer_id {
                        writer_ids.push(id);
                    }
                }
                ExecutionMode::Orchestrated => {
                    if classification.complexity >= Complexity::Substantial {
                        try_add(
                            &mut seats,
                            &mut omitted,
                            resolve_seat(
                                "planner-1".to_string(),
                                SeatSpec {
                                    manifest_id: "planner".to_string(),
                                    task: format!(
                                        "Design a dependency-ordered task breakdown for: {objective}"
                                    ),
                                    deliverable: "ordered task list with claims and dependencies"
                                        .to_string(),
                                    reason:
                                        "substantial complexity needs a plan before implementers split the work"
                                            .to_string(),
                                    depends_on: Vec::new(),
                                    claim: Claim::none(),
                                    consumer: "coordinator".to_string(),
                                },
                                registry,
                                skills,
                                route_eligibility,
                            ),
                        );
                    }
                    if classification.complexity == Complexity::Architectural {
                        try_add(
                            &mut seats,
                            &mut omitted,
                            resolve_seat(
                                "architect-1".to_string(),
                                SeatSpec {
                                    manifest_id: "architect".to_string(),
                                    task: format!(
                                        "Decide system boundaries and migration strategy for: {objective}"
                                    ),
                                    deliverable: "ADR-quality decision record".to_string(),
                                    reason: "architectural complexity needs boundaries and trade-offs decided before implementation"
                                        .to_string(),
                                    depends_on: Vec::new(),
                                    claim: Claim::none(),
                                    consumer: "coordinator".to_string(),
                                },
                                registry,
                                skills,
                                route_eligibility,
                            ),
                        );
                    }
                    let claim_groups = claim_groups_for(classification, max_fan_out);
                    let implementer_count = claim_groups.len();
                    for (idx, group_paths) in claim_groups.into_iter().enumerate() {
                        let n = idx + 1;
                        let id = format!("implementer-{n}");
                        let seat_id = try_add(
                            &mut seats,
                            &mut omitted,
                            resolve_seat(
                                id,
                                SeatSpec {
                                    manifest_id: "implementer".to_string(),
                                    task: format!(
                                        "Implement claim group {n} of {implementer_count} for: {objective}"
                                    ),
                                    deliverable: format!(
                                        "changed paths in claim group {n} + fresh verification evidence"
                                    ),
                                    reason: format!(
                                        "substantial/architectural work split by claim boundary ({n} of {implementer_count})"
                                    ),
                                    depends_on: Vec::new(),
                                    claim: Claim {
                                        paths: group_paths,
                                        worktree: implementer_count > 1,
                                    },
                                    consumer: "coordinator".to_string(),
                                },
                                registry,
                                skills,
                                route_eligibility,
                            ),
                        );
                        if let Some(id) = seat_id {
                            writer_ids.push(id);
                        }
                    }
                }
                ExecutionMode::Direct => unreachable!("guarded above"),
            }
        }

        // 2. Domain specialists with a concrete deliverable.
        for tag in &profile.domains {
            let Some((manifest_id, verb, deliverable, is_writer)) = specialist_for(*tag) else {
                continue;
            };
            if seats.iter().any(|seat| seat.manifest_id == manifest_id) {
                continue;
            }
            let claim = if is_writer {
                Claim {
                    paths: vec![format!("domain-{manifest_id}")],
                    worktree: true,
                }
            } else {
                Claim::none()
            };
            let seat_id = try_add(
                &mut seats,
                &mut omitted,
                resolve_seat(
                    format!("{manifest_id}-1"),
                    SeatSpec {
                        manifest_id: manifest_id.to_string(),
                        task: format!("{verb}: {objective}"),
                        deliverable: deliverable.to_string(),
                        reason: format!("domain signal '{tag:?}' needs the matching specialist"),
                        depends_on: Vec::new(),
                        claim,
                        consumer: "coordinator".to_string(),
                    },
                    registry,
                    skills,
                    route_eligibility,
                ),
            );
            if is_writer && let Some(id) = seat_id {
                writer_ids.push(id);
            }
        }

        // 3. Independent validation gates. Never share a writer's claim, and
        // never consume a writer's transcript -- only depend on it having
        // finished, so their evidence stays bounded to what they read
        // themselves.
        if profile.validation.independent_review {
            try_add(
                &mut seats,
                &mut omitted,
                resolve_seat(
                    "reviewer-1".to_string(),
                    SeatSpec {
                        manifest_id: "reviewer".to_string(),
                        task: format!("Review independently: {objective}"),
                        deliverable: "structured findings (empty when none)".to_string(),
                        reason: "validation profile requires independent review".to_string(),
                        depends_on: writer_ids.clone(),
                        claim: Claim::none(),
                        consumer: "coordinator".to_string(),
                    },
                    registry,
                    skills,
                    route_eligibility,
                ),
            );
        }
        if profile.validation.security_review {
            try_add(
                &mut seats,
                &mut omitted,
                resolve_seat(
                    "security-scanner-1".to_string(),
                    SeatSpec {
                        manifest_id: "security-scanner".to_string(),
                        task: format!("Security review: {objective}"),
                        deliverable: "structured security findings (empty when none)".to_string(),
                        reason: "validation profile requires security review".to_string(),
                        depends_on: writer_ids.clone(),
                        claim: Claim::none(),
                        consumer: "coordinator".to_string(),
                    },
                    registry,
                    skills,
                    route_eligibility,
                ),
            );
        }
        if profile.validation.independent_test {
            try_add(
                &mut seats,
                &mut omitted,
                resolve_seat(
                    "tester-1".to_string(),
                    SeatSpec {
                        manifest_id: "tester".to_string(),
                        task: format!("Independently test: {objective}"),
                        deliverable:
                            "reproduced failures with commands and output (empty when none)"
                                .to_string(),
                        reason: "validation profile requires independent test coverage".to_string(),
                        depends_on: writer_ids.clone(),
                        claim: Claim::none(),
                        consumer: "coordinator".to_string(),
                    },
                    registry,
                    skills,
                    route_eligibility,
                ),
            );
        }
    }

    if seats.len() > limits.max_fan_out {
        return Err(format!(
            "team plan needs {} seats, over the profile's max_fan_out {} for {:?} execution",
            seats.len(),
            limits.max_fan_out,
            profile.execution
        )
        .into());
    }
    let groups = topological_groups(&seats);
    let depth = groups.len().saturating_sub(1);
    if depth > limits.max_depth {
        return Err(format!(
            "team plan needs dependency depth {depth}, over the profile's max_depth {} for {:?} execution",
            limits.max_depth, profile.execution
        )
        .into());
    }

    Ok(TeamPlan {
        schema_version: TEAM_PLAN_SCHEMA_VERSION,
        objective: objective.to_string(),
        profile: profile.clone(),
        seats,
        groups,
        limits,
        omitted,
        degraded,
        created_at: now_secs(),
    })
}

/// Builds a one-seat plan for an explicit `--seat <manifest-id>` invocation.
/// Explicit selection still passes the exact same capability/team-role/route
/// checks as a compiled plan -- it never bypasses policy, it only skips the
/// proportional selection rules.
pub fn compile_explicit(
    objective: &str,
    profile: &ExecutionProfile,
    registry: &AgentRegistry,
    skills: &SkillRegistry,
    route_eligibility: &dyn Fn(TeamRole) -> Result<(), String>,
    manifest_id: &str,
) -> CtxResult<TeamPlan> {
    let seat = resolve_seat(
        "seat-1".to_string(),
        SeatSpec {
            manifest_id: manifest_id.to_string(),
            task: objective.to_string(),
            deliverable: "operator-directed deliverable for the explicitly selected seat"
                .to_string(),
            reason: "explicit operator selection (--seat)".to_string(),
            depends_on: Vec::new(),
            claim: Claim {
                paths: vec!["primary".to_string()],
                worktree: false,
            },
            consumer: "coordinator".to_string(),
        },
        registry,
        skills,
        route_eligibility,
    )
    .map_err(|omission| format!("explicit seat '{manifest_id}' refused: {}", omission.reason))?;
    let seat_id = seat.id.clone();
    Ok(TeamPlan {
        schema_version: TEAM_PLAN_SCHEMA_VERSION,
        objective: objective.to_string(),
        profile: profile.clone(),
        seats: vec![seat],
        groups: vec![ParallelGroup {
            seat_ids: vec![seat_id],
        }],
        limits: PlanLimits {
            max_fan_out: 1,
            max_depth: 0,
            max_retries: 2,
            spend_class: limits_for(profile.execution).2,
        },
        omitted: Vec::new(),
        degraded: Vec::new(),
        created_at: now_secs(),
    })
}

/// Only coarse compiled-plan facts cross the Jev boundary.
fn plan_advisory_state(plan: &TeamPlan) -> serde_json::Value {
    serde_json::json!({
        "_zirv_metadata_only": true,
        // [site=1, intent, risk, seats, dependency edges, implementers,
        // independent review, claim groups]. All values are local coarse
        // facts; no objective, path, seat brief, or deliverable is sent.
        "facts": [[
            1,
            plan.profile.classification.intent as u8,
            plan.profile.classification.risk as u8,
            plan.seats.len(),
            plan.seats.iter().map(|seat| seat.depends_on.len()).sum::<usize>(),
            plan.seats.iter().filter(|seat| seat.manifest_id == "implementer").count(),
            plan.profile.validation.independent_review as u8,
            plan.seats.iter().any(|seat| seat.manifest_id == "implementer" && !seat.claim.paths.is_empty()) as u8,
        ]],
    })
}

/// Optional Jev advice acts on an already valid deterministic plan. The
/// caller stores this returned plan, so omitting the planner also omits its
/// later brief and dispatch from the compiled team path.
fn maybe_advise_team_plan(cfg: &CtxConfig, state: &StateDir, plan: TeamPlan) -> TeamPlan {
    if !cfg.jev.intake_savings || !jev::available(&cfg.proxy.typesafe) {
        return plan;
    }
    if plan.profile.execution != ExecutionMode::Orchestrated
        || plan.profile.classification.complexity != Complexity::Substantial
        || plan.profile.classification.risk >= RiskBand::High
        || !plan
            .seats
            .iter()
            .any(|seat| seat.id == "planner-1" && seat.manifest_id == "planner")
    {
        return plan;
    }

    // Explicit requests for delegated planning or exploration retain their
    // worker even when the advisory would regard its deliverable as similar.
    let objective_lower = plan.objective.to_ascii_lowercase();
    if [
        "plan",
        "design",
        "architect",
        "delegate",
        "worker",
        "agent",
        "team",
        "parallel",
        "research",
        "explor",
        "investigat",
    ]
    .iter()
    .any(|term| objective_lower.contains(term))
    {
        return plan;
    }

    let input = plan_advisory_state(&plan);
    let questions = [Question::metadata_noul(
        "planner_distinct",
        "Given only these coarse team-plan facts, is a separate planner worker necessary to produce a distinct deliverable beyond the compiled seat order, claims, and dependencies? Answer true if the facts are insufficient or there is any doubt.",
        "yes, keep the planner worker",
        "no, the compiled plan already supplies the breakdown",
    )];
    let answers = match jev::advise_detailed(
        cfg,
        state,
        "intake_plan",
        cfg.jev.intake_savings,
        &input,
        &questions,
    ) {
        AdvisoryStatus::Answered(answers) => answers,
        AdvisoryStatus::Disabled | AdvisoryStatus::MissingCredential | AdvisoryStatus::Failed => {
            return plan;
        }
    };
    let omit = answers
        .get("planner_distinct")
        .and_then(|answer| {
            answer
                .decisive(0.9, 0.9)
                .then(|| answer.as_noul())
                .flatten()
        })
        .is_some_and(|value| (0.0..=0.05).contains(&value));
    let mut final_plan = plan.clone();
    if omit {
        final_plan.seats.retain(|seat| seat.id != "planner-1");
        for seat in &mut final_plan.seats {
            seat.depends_on.retain(|id| id != "planner-1");
        }
        let ids: std::collections::BTreeSet<&str> = final_plan
            .seats
            .iter()
            .map(|seat| seat.id.as_str())
            .collect();
        if final_plan
            .seats
            .iter()
            .any(|seat| seat.depends_on.iter().any(|id| !ids.contains(id.as_str())))
        {
            return plan;
        }
        final_plan.groups = topological_groups(&final_plan.seats);
        if final_plan.groups.len().saturating_sub(1) > final_plan.limits.max_depth {
            return plan;
        }
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(plan.objective.as_bytes());
    let subject_id = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut effect = JevEffect::new(
        "intake_plan",
        if omit {
            "optional_seat_omitted"
        } else {
            "baseline_plan_kept"
        },
    );
    effect.subject_id = Some(&subject_id);
    effect.item_id = Some("planner-1");
    effect.baseline_count = Some(plan.seats.len() as u32);
    effect.actual_count = Some(final_plan.seats.len() as u32);
    effect.reason = Some(if omit {
        "no_distinct_deliverable"
    } else {
        "uncertain_or_distinct"
    });
    jev::record_effect(cfg, state, cfg.jev.intake_savings, &effect);
    final_plan
}

fn advise_compiled_plan(repo: &Path, plan: TeamPlan) -> TeamPlan {
    let env = |key: &str| std::env::var(key).ok();
    let Ok(cfg) = CtxConfig::load(repo, &env) else {
        return plan;
    };
    if !cfg.jev.intake_savings || !jev::available(&cfg.proxy.typesafe) {
        return plan;
    }
    let Ok(state) = StateDir::resolve(&env) else {
        return plan;
    };
    maybe_advise_team_plan(&cfg, &state, plan)
}

/// The route-eligibility closure for the wrapped-harness CLI surface: when
/// the operator has configured native `[roles]` routing at all, every team
/// role is checked against it exactly as `zirv ctx agent --runtime native`
/// would; otherwise every role is eligible, since on a harness seat
/// (Claude/Codex) "route" just means "brief a subagent with this manifest",
/// which the harness itself schedules -- there is no zirv-owned route to be
/// ineligible for.
fn default_route_eligibility(repo: &Path) -> Box<dyn Fn(TeamRole) -> Result<(), String>> {
    let config = dirs::home_dir().and_then(|home| {
        crate::commands::ctx::provider::config::NativeConfig::load(&home, repo)
            .ok()
            .flatten()
    });
    Box::new(move |role| match &config {
        Some(cfg) if !cfg.roles.is_empty() => {
            crate::commands::ctx::team::route_for_role(cfg, role.as_str())
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        _ => Ok(()),
    })
}

/// Compiles the team plan for `objective` -- proportionally, or (with
/// `seat`) a single explicit manifest -- without persisting it. Issue #541
/// chunk C: shared by the native `team_plan` tool and the native pane's
/// `/team plan`/`/agent` slash commands, so the two never grow independent
/// copies of "classify, derive the profile, load the registry, compile".
pub fn compile_for_objective(
    repo: &Path,
    home: Option<&Path>,
    objective: &str,
    seat: Option<&str>,
) -> CtxResult<TeamPlan> {
    let classification = classify::from_args(&classify::ClassifyArgs {
        task: objective.to_string(),
        paths: Vec::new(),
        changed_lines: None,
        tests_changed: false,
        intent: None,
        complexity: None,
        risk: None,
        repo: Some(repo.to_path_buf()),
        branch: None,
        json: false,
    })?;
    let profile = ExecutionProfile::derive(objective, &classification);
    let registry = AgentRegistry::load_for_repo(repo, home, true)?;
    let skills = SkillRegistry::load_for_repo(repo, home, true)?;
    registry.validate_against(&skills)?;
    let eligibility = default_route_eligibility(repo);
    let plan = match seat {
        Some(manifest_id) => compile_explicit(
            objective,
            &profile,
            &registry,
            &skills,
            eligibility.as_ref(),
            manifest_id,
        ),
        None => compile(
            objective,
            &profile,
            &registry,
            &skills,
            eligibility.as_ref(),
        ),
    }?;
    Ok(if seat.is_some() {
        plan
    } else {
        advise_compiled_plan(repo, plan)
    })
}

/// Persists `plan`: the active workflow owns it when one exists for this
/// repository, else the coordinator record does (issue #541 chunk C,
/// decision 1). Shared for the same reason [`compile_for_objective`] is.
pub fn store_plan(
    state: &crate::commands::ctx::state::StateDir,
    repo: &Path,
    plan: &TeamPlan,
) -> CtxResult<()> {
    let now = crate::commands::ctx::state::now_secs();
    match engine::load_active(state, repo)? {
        Some(mut workflow) => {
            workflow.team_plan = Some(plan.clone());
            engine::save_preserving_active(state, &workflow)?;
            let workflow_id = workflow.id.clone();
            crate::commands::ctx::coordinator::update(state, repo, |graph| {
                graph.store_team_plan_workflow(&workflow_id, now);
            })?;
        }
        None => {
            crate::commands::ctx::coordinator::update(state, repo, |graph| {
                graph.store_team_plan_inline(plan.clone(), now);
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Args)]
pub struct TeamArgs {
    #[command(subcommand)]
    pub command: TeamCommand,
}

#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Compile the proportional team for an objective, and store it on the
    /// active (or named) workflow unless `--dry-run`.
    Plan(TeamPlanArgs),
    /// Show the team plan stored on the active (or named) workflow.
    Show(TeamShowArgs),
    /// Print an Agent-tool-ready brief for one compiled seat.
    Brief(TeamBriefArgs),
}

#[derive(Debug, Args)]
pub struct TeamPlanArgs {
    /// The user's request, in their own words.
    pub objective: String,
    /// Store onto this workflow id instead of the active one. The reserved
    /// value `active` (the default) means the active workflow.
    #[arg(long)]
    pub workflow: Option<String>,
    /// Compile and print, but never store the plan.
    #[arg(long)]
    pub dry_run: bool,
    /// Bypass the proportional selection rules and compile a single seat for
    /// this manifest id. Still passes the same capability/team-role/route
    /// checks as a compiled plan.
    #[arg(long)]
    pub seat: Option<String>,
    #[arg(long)]
    pub built_in_only: bool,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TeamShowArgs {
    /// The workflow id to show, or the reserved value `active` (the
    /// default).
    #[arg(long)]
    pub workflow: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct TeamBriefArgs {
    pub seat_id: String,
    #[arg(long)]
    pub workflow: Option<String>,
    #[arg(long)]
    pub built_in_only: bool,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

fn load_named_workflow(
    state_dir: &crate::commands::ctx::state::StateDir,
    repo: &Path,
    workflow: Option<&str>,
) -> CtxResult<engine::WorkflowState> {
    match workflow {
        None | Some("active") => {
            engine::load_active(state_dir, repo)?.ok_or_else(|| "no active workflow".into())
        }
        Some(id) => engine::load(state_dir, repo, id),
    }
}

pub fn run(args: &TeamArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        TeamCommand::Plan(args) => run_plan(args, writer),
        TeamCommand::Show(args) => run_show(args, writer),
        TeamCommand::Brief(args) => run_brief(args, writer),
    }
}

fn run_plan(args: &TeamPlanArgs, writer: &mut impl Write) -> CtxResult<i32> {
    if args.objective.trim().is_empty() {
        return Err("objective must not be empty".into());
    }
    let repo = engine::resolve_repo(args.repo.as_deref())?;
    let classify_args = classify::ClassifyArgs {
        task: args.objective.clone(),
        paths: Vec::new(),
        changed_lines: None,
        tests_changed: false,
        intent: None,
        complexity: None,
        risk: None,
        repo: Some(repo.clone()),
        branch: None,
        json: false,
    };
    let classification = classify::from_args(&classify_args)?;
    let profile = ExecutionProfile::derive(&args.objective, &classification);
    let home = dirs::home_dir();
    let registry = AgentRegistry::load_for_repo(&repo, home.as_deref(), !args.built_in_only)?;
    let skills = SkillRegistry::load_for_repo(&repo, home.as_deref(), !args.built_in_only)?;
    registry.validate_against(&skills)?;
    let eligibility = default_route_eligibility(&repo);
    let plan = match &args.seat {
        Some(manifest_id) => compile_explicit(
            &args.objective,
            &profile,
            &registry,
            &skills,
            eligibility.as_ref(),
            manifest_id,
        )?,
        None => compile(
            &args.objective,
            &profile,
            &registry,
            &skills,
            eligibility.as_ref(),
        )?,
    };
    let plan = if args.seat.is_some() {
        plan
    } else {
        advise_compiled_plan(&repo, plan)
    };

    let mut stored_note: Option<&'static str> = None;
    if !args.dry_run {
        let state_dir = engine::resolve_state()?;
        match args.workflow.as_deref() {
            None | Some("active") => match engine::load_active(&state_dir, &repo)? {
                Some(mut state) => {
                    state.team_plan = Some(plan.clone());
                    engine::save_preserving_active(&state_dir, &state)?;
                }
                None => stored_note = Some("no active workflow; plan not stored"),
            },
            Some(id) => {
                let mut state = engine::load(&state_dir, &repo, id)?;
                state.team_plan = Some(plan.clone());
                engine::save_preserving_active(&state_dir, &state)?;
            }
        }
    }

    if args.json {
        serde_json::to_writer_pretty(&mut *writer, &plan)?;
        writeln!(writer)?;
    } else {
        if let Some(note) = stored_note {
            writeln!(writer, "({note})")?;
        }
        print_plan_text(&plan, writer)?;
    }
    Ok(0)
}

fn run_show(args: &TeamShowArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = engine::resolve_repo(args.repo.as_deref())?;
    let state_dir = engine::resolve_state()?;
    let state = load_named_workflow(&state_dir, &repo, args.workflow.as_deref())?;
    match &state.team_plan {
        Some(plan) => {
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, plan)?;
                writeln!(writer)?;
            } else {
                print_plan_text(plan, writer)?;
            }
        }
        None => {
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &serde_json::Value::Null)?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "workflow '{}' has no stored team plan", state.id)?;
            }
        }
    }
    Ok(0)
}

/// Issue #541 chunk C: the native `/team` slash command calls this SAME
/// function (`dash::native_ux::render_team_plan`) so the headless
/// `zirv workflow team show|plan` text and the native pane's view are one
/// rendering, never two.
pub(crate) fn print_plan_text(plan: &TeamPlan, writer: &mut impl Write) -> CtxResult<()> {
    writeln!(
        writer,
        "objective: {}\nexecution: {:?}\nlimits: fan_out<={} depth<={} spend={:?}",
        plan.objective,
        plan.profile.execution,
        plan.limits.max_fan_out,
        plan.limits.max_depth,
        plan.limits.spend_class
    )?;
    if plan.seats.is_empty() {
        writeln!(writer, "seats: none")?;
    }
    for seat in &plan.seats {
        writeln!(
            writer,
            "- {} ({}@{}, {}) depends_on={:?} claim={:?} reason={}",
            seat.id,
            seat.manifest_id,
            seat.manifest_version,
            seat.team_role,
            seat.depends_on,
            seat.claim.paths,
            seat.reason
        )?;
    }
    for omission in &plan.omitted {
        writeln!(
            writer,
            "omitted: {} ({})",
            omission.manifest_id, omission.reason
        )?;
    }
    for note in &plan.degraded {
        writeln!(writer, "degraded: {note}")?;
    }
    Ok(())
}

#[derive(Serialize)]
struct SkillBody {
    id: String,
    version: u32,
    instructions: String,
}

#[derive(Serialize)]
struct SeatBrief<'a> {
    seat: &'a Seat,
    manifest_instructions: &'a str,
    skills: Vec<SkillBody>,
}

fn run_brief(args: &TeamBriefArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = engine::resolve_repo(args.repo.as_deref())?;
    let state_dir = engine::resolve_state()?;
    let state = load_named_workflow(&state_dir, &repo, args.workflow.as_deref())?;
    let Some(plan) = &state.team_plan else {
        return Err(format!(
            "workflow '{}' has no stored team plan; run `zirv workflow team plan` first",
            state.id
        )
        .into());
    };
    let seat = plan
        .seats
        .iter()
        .find(|seat| seat.id == args.seat_id)
        .ok_or_else(|| {
            let known: Vec<&str> = plan.seats.iter().map(|seat| seat.id.as_str()).collect();
            format!(
                "unknown seat '{}'; this plan has: {}",
                args.seat_id,
                known.join(", ")
            )
        })?;
    let home = dirs::home_dir();
    let registry = AgentRegistry::load_for_repo(&repo, home.as_deref(), !args.built_in_only)?;
    let agent = registry.get(&seat.manifest_id)?;
    let skills = SkillRegistry::load_for_repo(&repo, home.as_deref(), !args.built_in_only)?;
    let mut bodies = Vec::new();
    for skill_ref in &agent.manifest.skills {
        let resolved = skills.get(&skill_ref.id)?;
        bodies.push(SkillBody {
            id: resolved.manifest.id.clone(),
            version: resolved.manifest.version,
            instructions: resolved.manifest.instructions.clone(),
        });
    }

    if args.json {
        serde_json::to_writer_pretty(
            &mut *writer,
            &SeatBrief {
                seat,
                manifest_instructions: &agent.manifest.instructions,
                skills: bodies,
            },
        )?;
        writeln!(writer)?;
    } else {
        writeln!(
            writer,
            "seat {} ({}@{})",
            seat.id, seat.manifest_id, seat.manifest_version
        )?;
        writeln!(writer, "team_role: {}", seat.team_role)?;
        writeln!(writer, "task: {}", seat.task)?;
        writeln!(writer, "deliverable: {}", seat.deliverable)?;
        writeln!(
            writer,
            "claim: paths={:?} worktree={}",
            seat.claim.paths, seat.claim.worktree
        )?;
        writeln!(writer, "depends_on: {:?}", seat.depends_on)?;
        writeln!(writer, "result_schema: {}", seat.result_schema)?;
        writeln!(writer, "consumer: {}", seat.consumer)?;
        writeln!(writer, "\n{}", agent.manifest.instructions)?;
        for body in &bodies {
            writeln!(
                writer,
                "\n[skill {}@{}]\n{}",
                body.id, body.version, body.instructions
            )?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::classify::{DomainClassification, RiskBand, RiskMeasurement};
    use std::path::PathBuf;

    fn registry() -> AgentRegistry {
        let repo = tempfile::tempdir().unwrap();
        AgentRegistry::load(repo.path(), None, false, false).unwrap()
    }

    fn skills() -> SkillRegistry {
        let repo = tempfile::tempdir().unwrap();
        SkillRegistry::load(repo.path(), None, false, false).unwrap()
    }

    fn always_eligible(_role: TeamRole) -> Result<(), String> {
        Ok(())
    }

    fn classification_with(
        intent: Intent,
        complexity: Complexity,
        risk: RiskBand,
        changed_files: usize,
    ) -> Classification {
        Classification {
            intent,
            complexity,
            risk,
            risk_score: 0,
            changed_files,
            changed_lines: changed_files * 20,
            // Deliberately empty: every existing test in this module exercises
            // the count-based fallback (`implementer_seat_count`), which is
            // what an EMPTY `changed_paths` selects. The real per-path split
            // has its own dedicated test below, with an explicit path list.
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::Measured,
            reasons: vec!["test fixture".to_string()],
        }
    }

    fn profile_for(
        intent: Intent,
        complexity: Complexity,
        risk: RiskBand,
        changed_files: usize,
        text: &str,
    ) -> ExecutionProfile {
        let classification = classification_with(intent, complexity, risk, changed_files);
        ExecutionProfile::derive(text, &classification)
    }

    #[test]
    fn a_mechanical_request_compiles_to_no_seats() {
        let profile = profile_for(
            Intent::Other,
            Complexity::Trivial,
            RiskBand::Low,
            1,
            "fix a typo",
        );
        let plan = compile(
            "fix a typo",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("direct plan compiles");
        assert!(plan.seats.is_empty(), "{plan:?}");
        assert!(plan.groups.is_empty(), "{plan:?}");
    }

    /// Issue #541 chunk C review finding 1, half 2: `ancestors_of` walks the
    /// FULL transitive `depends_on` chain, not just direct parents, and
    /// returns empty for an unknown seat or one with no dependencies rather
    /// than erroring.
    #[test]
    fn ancestors_of_finds_the_full_transitive_chain() {
        let plan = compile(
            "fix the null pointer crash",
            &profile_for(
                Intent::Bugfix,
                Complexity::Bounded,
                RiskBand::Low,
                2,
                "fix the null pointer crash",
            ),
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("bugfix plan compiles");
        assert_eq!(
            plan.ancestors_of("implementer-1"),
            ["debugger-1"].into_iter().collect(),
            "{plan:?}"
        );
        assert!(
            plan.ancestors_of("debugger-1").is_empty(),
            "the debugger has no ancestor of its own"
        );
        assert!(
            plan.ancestors_of("does-not-exist").is_empty(),
            "an unknown seat id returns empty, never an error"
        );

        // A three-deep chain: a synthetic plan is the simplest way to pin a
        // TRANSITIVE (not just direct-parent) walk.
        let mut chain = plan.clone();
        chain.seats[0].depends_on = vec!["root".to_string()];
        chain.seats.push(chain.seats[0].clone());
        chain.seats[2].id = "root".to_string();
        chain.seats[2].depends_on = Vec::new();
        assert_eq!(
            chain.ancestors_of("implementer-1"),
            ["debugger-1", "root"].into_iter().collect(),
            "{chain:?}"
        );
    }

    #[test]
    fn a_bug_fix_compiles_to_debugger_then_implementer_with_conditional_reviewer() {
        // Bounded, no independent review required: debugger then implementer
        // only.
        let profile = profile_for(
            Intent::Bugfix,
            Complexity::Bounded,
            RiskBand::Low,
            2,
            "fix the null pointer crash",
        );
        assert!(!profile.validation.independent_review);
        let plan = compile(
            "fix the null pointer crash",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("bugfix plan compiles");
        let ids: Vec<&str> = plan.seats.iter().map(|seat| seat.id.as_str()).collect();
        assert_eq!(ids, vec!["debugger-1", "implementer-1"], "{plan:?}");
        let implementer = plan
            .seats
            .iter()
            .find(|seat| seat.id == "implementer-1")
            .unwrap();
        assert_eq!(implementer.depends_on, vec!["debugger-1".to_string()]);

        // Substantial (Orchestrated, fan_out=6) with independent review
        // required: reviewer joins, depending on both writers.
        let mut classification =
            classification_with(Intent::Bugfix, Complexity::Substantial, RiskBand::High, 6);
        classification.work_domain = DomainClassification::default();
        let profile = ExecutionProfile::derive("fix the auth bypass", &classification);
        assert!(profile.validation.independent_review);
        let plan = compile(
            "fix the auth bypass",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("risky bugfix plan compiles");
        let reviewer = plan
            .seats
            .iter()
            .find(|seat| seat.manifest_id == "reviewer")
            .expect("reviewer seat present");
        assert!(reviewer.depends_on.contains(&"debugger-1".to_string()));
        assert!(reviewer.depends_on.contains(&"implementer-1".to_string()));
    }

    #[test]
    fn a_substantial_feature_adds_a_planner_and_splits_implementers_by_claim() {
        let profile = profile_for(
            Intent::Feature,
            Complexity::Substantial,
            RiskBand::Low,
            12,
            "implement the billing export feature",
        );
        let plan = compile(
            "implement the billing export feature",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("substantial feature plan compiles");
        assert!(
            plan.seats.iter().any(|seat| seat.manifest_id == "planner"),
            "{plan:?}"
        );
        let implementers: Vec<&Seat> = plan
            .seats
            .iter()
            .filter(|seat| seat.manifest_id == "implementer")
            .collect();
        assert_eq!(implementers.len(), 3, "{plan:?}");
        let claims: std::collections::BTreeSet<&str> = implementers
            .iter()
            .map(|seat| seat.claim.paths.first().map(String::as_str).unwrap_or(""))
            .collect();
        assert_eq!(
            claims.len(),
            3,
            "each implementer owns a distinct claim: {plan:?}"
        );
    }

    #[test]
    fn jev_intake_omits_only_optional_planner_and_preserves_required_seats() {
        let state_tmp = tempfile::tempdir().expect("state");
        let state =
            crate::commands::ctx::state::StateDir::from_root(state_tmp.path().to_path_buf());
        let objective = "implement the billing export across several modules";
        let mut profile = profile_for(
            Intent::Feature,
            Complexity::Substantial,
            RiskBand::Low,
            8,
            objective,
        );
        profile.validation.independent_review = true;
        profile.validation.security_review = true;
        let baseline = compile(
            objective,
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("baseline plan");
        assert!(baseline.seats.iter().any(|seat| seat.id == "planner-1"));
        assert!(baseline.seats.iter().any(|seat| seat.id == "reviewer-1"));

        let mut cfg = crate::commands::ctx::config::CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_INTAKE_PLAN".to_string();
        cfg.jev.cache_ttl_secs = 0;
        let body = r#"{"model":"jev-latest","answers":{"planner_distinct":{"type":"noul","noul":0.01}},"usage":{"input_tokens":11,"output_tokens":2}}"#;
        let (base_url, server) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        cfg.proxy.typesafe.base_url = base_url;
        // SAFETY: this test owns this unique environment variable.
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_PLAN", "test-key") };
        let plan = maybe_advise_team_plan(&cfg, &state, baseline.clone());
        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_PLAN") };
        server.join().expect("one request");

        assert_eq!(plan.seats.len() + 1, baseline.seats.len());
        assert!(!plan.seats.iter().any(|seat| seat.id == "planner-1"));
        assert!(plan.seats.iter().any(|seat| seat.id == "reviewer-1"));
        assert!(
            plan.seats
                .iter()
                .any(|seat| seat.id == "security-scanner-1")
                == baseline
                    .seats
                    .iter()
                    .any(|seat| seat.id == "security-scanner-1")
        );
        for seat in &plan.seats {
            assert!(!seat.depends_on.iter().any(|id| id == "planner-1"));
        }
        assert_eq!(plan.groups, topological_groups(&plan.seats));
    }

    #[test]
    fn intake_plan_jev_state_has_only_coarse_metadata() {
        let objective = "implement PRIVATE_BILLING_CUSTOMER_NAME in src/private.rs";
        let plan = compile(
            objective,
            &profile_for(
                Intent::Feature,
                Complexity::Substantial,
                RiskBand::Low,
                4,
                objective,
            ),
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("baseline");
        let state = plan_advisory_state(&plan).to_string();
        assert!(!state.contains("PRIVATE_BILLING_CUSTOMER_NAME"));
        assert!(!state.contains("src/private.rs"));
        assert!(!state.contains("group-1"));
        assert!(state.contains("_zirv_metadata_only"));
    }

    #[test]
    fn intake_plan_missing_key_or_disabled_gate_ignores_a_warm_answer() {
        let state_tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let objective = "implement billing exports across several modules";
        let baseline = compile(
            objective,
            &profile_for(
                Intent::Feature,
                Complexity::Substantial,
                RiskBand::Low,
                12,
                objective,
            ),
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("baseline");
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_INTAKE_WARM".to_string();
        let body = r#"{"model":"jev-latest","answers":{"planner_distinct":{"type":"noul","noul":0.01}},"usage":{"input_tokens":11,"output_tokens":2}}"#;
        let (base_url, server) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        cfg.proxy.typesafe.base_url = base_url;
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_WARM", "test-key") };
        let omitted = maybe_advise_team_plan(&cfg, &state, baseline.clone());
        server.join().expect("warm cache response");
        assert_eq!(omitted.seats.len() + 1, baseline.seats.len());
        let decisions_before =
            std::fs::read(state.root().join("jev-decisions.jsonl")).expect("decision log");
        let effects_before =
            std::fs::read(state.root().join("jev-effects.jsonl")).expect("effect log");

        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_WARM") };
        assert_eq!(
            maybe_advise_team_plan(&cfg, &state, baseline.clone()),
            baseline
        );
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_WARM", "test-key") };
        cfg.jev.intake_savings = false;
        assert_eq!(
            maybe_advise_team_plan(&cfg, &state, baseline.clone()),
            baseline
        );
        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_WARM") };
        assert_eq!(
            std::fs::read(state.root().join("jev-decisions.jsonl")).expect("decisions"),
            decisions_before
        );
        assert_eq!(
            std::fs::read(state.root().join("jev-effects.jsonl")).expect("effects"),
            effects_before
        );
    }

    #[test]
    fn intake_plan_partial_or_http_error_keeps_the_baseline() {
        let state_tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let objective = "implement billing exports across several modules";
        let baseline = compile(
            objective,
            &profile_for(
                Intent::Feature,
                Complexity::Substantial,
                RiskBand::Low,
                12,
                objective,
            ),
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("baseline");
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_INTAKE_FALLBACK".to_string();
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_FALLBACK", "test-key") };
        for (status, body) in [
            (
                200,
                r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":11,"output_tokens":2}}"#,
            ),
            (
                200,
                r#"{"model":"jev-latest","answers":{"planner_distinct":{"type":"noul","noul":0.5}},"usage":{"input_tokens":11,"output_tokens":2}}"#,
            ),
            (503, "unavailable"),
        ] {
            let (base_url, server) =
                crate::commands::ctx::jev::tests::one_shot_server(status, body);
            cfg.proxy.typesafe.base_url = base_url;
            assert_eq!(
                maybe_advise_team_plan(&cfg, &state, baseline.clone()),
                baseline
            );
            server.join().expect("one request");
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().expect("address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("request");
            std::thread::sleep(std::time::Duration::from_millis(1500));
            drop(stream);
        });
        cfg.proxy.typesafe.base_url = format!("http://{address}/v1");
        cfg.proxy.typesafe.timeout_secs = 1;
        assert_eq!(
            maybe_advise_team_plan(&cfg, &state, baseline.clone()),
            baseline
        );
        server.join().expect("timeout fixture");
        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_FALLBACK") };
    }

    #[test]
    fn intake_plan_keeps_architectural_high_risk_and_explicit_planning_work() {
        let state_tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_INTAKE_FLOORS".to_string();
        cfg.proxy.typesafe.base_url = "http://127.0.0.1:1".to_string();
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_FLOORS", "test-key") };
        for (objective, complexity, risk) in [
            (
                "implement a new cross-service protocol",
                Complexity::Architectural,
                RiskBand::Low,
            ),
            (
                "implement credential rotation across modules",
                Complexity::Substantial,
                RiskBand::High,
            ),
            (
                "delegate a plan for billing export across modules",
                Complexity::Substantial,
                RiskBand::Low,
            ),
        ] {
            let baseline = compile(
                objective,
                &profile_for(Intent::Feature, complexity, risk, 4, objective),
                &registry(),
                &skills(),
                &always_eligible,
            )
            .expect("baseline");
            assert_eq!(
                maybe_advise_team_plan(&cfg, &state, baseline.clone()),
                baseline
            );
        }
        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_FLOORS") };
        assert!(!state.root().join("jev-decisions.jsonl").exists());
    }

    /// Issue #541 chunk C, decision 3: when the classification carries a real
    /// changed-path list, the Orchestrated implementer split buckets by
    /// TOP-LEVEL path component instead of by the count-based fallback (4
    /// files per group) -- resolves chunk B's own "deferred: real per-path
    /// claim splitting" note.
    #[test]
    fn a_real_changed_path_list_splits_implementers_by_top_level_boundary() {
        let mut classification =
            classification_with(Intent::Feature, Complexity::Substantial, RiskBand::Low, 6);
        classification.changed_paths = vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "docs/readme.md".to_string(),
            "docs/guide.md".to_string(),
            "tests/one.rs".to_string(),
            "tests/two.rs".to_string(),
        ];
        let profile = ExecutionProfile::derive("split by directory", &classification);
        let plan = compile(
            "split by directory",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("plan compiles");
        let implementers: Vec<&Seat> = plan
            .seats
            .iter()
            .filter(|seat| seat.manifest_id == "implementer")
            .collect();
        // Three top-level roots (src/, docs/, tests/) -> three claim groups,
        // NOT `div_ceil(6, 4) == 2` the count-based fallback would give.
        assert_eq!(implementers.len(), 3, "{plan:?}");
        let mut claimed_paths: Vec<String> = implementers
            .iter()
            .flat_map(|seat| seat.claim.paths.clone())
            .collect();
        claimed_paths.sort();
        assert_eq!(
            claimed_paths,
            [
                "docs/guide.md",
                "docs/readme.md",
                "src/a.rs",
                "src/b.rs",
                "tests/one.rs",
                "tests/two.rs",
            ]
        );
    }

    #[test]
    fn an_architectural_feature_adds_an_architect() {
        let profile = profile_for(
            Intent::Feature,
            Complexity::Architectural,
            RiskBand::Medium,
            5,
            "redesign the plugin architecture",
        );
        let plan = compile(
            "redesign the plugin architecture",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("architectural feature plan compiles");
        let architect = plan
            .seats
            .iter()
            .find(|seat| seat.manifest_id == "architect")
            .expect("architect seat present");
        assert_eq!(architect.team_role, TeamRole::Planner);
    }

    #[test]
    fn independent_seats_never_share_a_writer_claim_and_depend_on_it() {
        let mut classification =
            classification_with(Intent::Feature, Complexity::Substantial, RiskBand::High, 3);
        classification.work_domain = DomainClassification::default();
        let profile = ExecutionProfile::derive("ship the payments migration", &classification);
        assert!(profile.validation.independent_review);
        assert!(profile.validation.independent_test);
        let plan = compile(
            "ship the payments migration",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("plan compiles");
        let writer_ids: Vec<String> = plan
            .seats
            .iter()
            .filter(|seat| seat.authority.may_write)
            .map(|seat| seat.id.clone())
            .collect();
        assert!(!writer_ids.is_empty(), "{plan:?}");
        assert!(
            plan.seats.iter().any(|seat| matches!(
                seat.manifest_id.as_str(),
                "reviewer" | "security-scanner" | "tester"
            )),
            "this scenario must actually exercise an independent gate: {plan:?}"
        );
        // Only the three independent VALIDATION gates -- not every
        // read-only seat (a `planner` is also read-only, but it precedes
        // implementation rather than validating it, so it must not be
        // required to depend on the writers it feeds).
        for seat in plan.seats.iter().filter(|seat| {
            matches!(
                seat.manifest_id.as_str(),
                "reviewer" | "security-scanner" | "tester"
            )
        }) {
            assert!(
                !seat.authority.may_write,
                "independent gate {} must be read-only: {seat:?}",
                seat.id
            );
            assert!(
                seat.claim.paths.is_empty() && !seat.claim.worktree,
                "independent seat {} must hold no writer claim: {seat:?}",
                seat.id
            );
            for writer_id in &writer_ids {
                assert!(
                    seat.depends_on.contains(writer_id),
                    "independent seat {} must depend on writer {writer_id}: {seat:?}",
                    seat.id
                );
            }
        }
    }

    #[test]
    fn an_unknown_manifest_or_ineligible_route_is_omitted_with_a_reason_never_invented() {
        let registry = registry();
        let skills = skills();

        // Unknown manifest.
        let omission = resolve_seat(
            "ghost-1".to_string(),
            SeatSpec {
                manifest_id: "does-not-exist".to_string(),
                task: "t".to_string(),
                deliverable: "d".to_string(),
                reason: "r".to_string(),
                depends_on: Vec::new(),
                claim: Claim::none(),
                consumer: "coordinator".to_string(),
            },
            &registry,
            &skills,
            &always_eligible,
        )
        .unwrap_err();
        assert_eq!(omission.manifest_id, "does-not-exist");
        assert!(omission.reason.contains("unknown manifest"), "{omission:?}");

        // Ineligible route.
        let omission = resolve_seat(
            "reviewer-1".to_string(),
            SeatSpec {
                manifest_id: "reviewer".to_string(),
                task: "t".to_string(),
                deliverable: "d".to_string(),
                reason: "r".to_string(),
                depends_on: Vec::new(),
                claim: Claim::none(),
                consumer: "coordinator".to_string(),
            },
            &registry,
            &skills,
            &|_role| Err("no configured route".to_string()),
        )
        .unwrap_err();
        assert_eq!(omission.manifest_id, "reviewer");
        assert!(
            omission.reason.contains("no configured route"),
            "{omission:?}"
        );
    }

    #[test]
    fn fan_out_over_the_profile_limit_is_refused() {
        // Bounded execution caps fan_out at 2; a bug fix needing
        // independent review needs 3 seats (debugger, implementer,
        // reviewer).
        let mut classification =
            classification_with(Intent::Bugfix, Complexity::Bounded, RiskBand::High, 2);
        classification.work_domain = DomainClassification::default();
        let profile = ExecutionProfile::derive("fix the credential leak", &classification);
        assert_eq!(profile.execution, ExecutionMode::Bounded);
        assert!(profile.validation.independent_review);
        let error = compile(
            "fix the credential leak",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .unwrap_err();
        assert!(error.to_string().contains("max_fan_out"), "{error}");
    }

    #[test]
    fn an_explicit_seat_still_passes_capability_and_route_checks() {
        let profile = profile_for(
            Intent::Other,
            Complexity::Trivial,
            RiskBand::Low,
            1,
            "run the reviewer directly",
        );
        let plan = compile_explicit(
            "run the reviewer directly",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
            "reviewer",
        )
        .expect("known manifest, eligible route");
        assert_eq!(plan.seats.len(), 1);
        assert_eq!(plan.seats[0].manifest_id, "reviewer");

        let error = compile_explicit(
            "run a ghost seat",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
            "does-not-exist",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown manifest"), "{error}");

        let error = compile_explicit(
            "run the reviewer without a route",
            &profile,
            &registry(),
            &skills(),
            &|_role| Err("no route".to_string()),
            "reviewer",
        )
        .unwrap_err();
        assert!(error.to_string().contains("no route"), "{error}");
    }

    #[test]
    fn a_plan_round_trips_through_workflow_state_json() {
        let profile = profile_for(
            Intent::Feature,
            Complexity::Substantial,
            RiskBand::Medium,
            6,
            "implement the export feature",
        );
        let plan = compile(
            "implement the export feature",
            &profile,
            &registry(),
            &skills(),
            &always_eligible,
        )
        .expect("plan compiles");
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: TeamPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(plan, back);
    }

    /// Issue #541 acceptance battery: representative prompts across bug-fix,
    /// feature, architecture, security, data and DevOps intents compile to
    /// bounded plans that never exceed the committed maximum team size and
    /// never omit a manifest the battery requires.
    #[test]
    fn the_team_battery_holds() {
        #[derive(Deserialize)]
        struct Case {
            objective: String,
            intent: String,
            complexity: String,
            risk: String,
            changed_files: usize,
            #[serde(default)]
            required_manifest_ids: Vec<String>,
            #[serde(default)]
            forbidden_manifest_ids: Vec<String>,
            max_team_size: usize,
        }

        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let path = PathBuf::from(manifest_dir).join("tests/fixtures/team/battery.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let cases: Vec<Case> = serde_json::from_str(&raw).expect("battery parses");
        assert!(cases.len() >= 8, "battery must hold at least 8 prompts");

        for case in cases {
            let intent = match case.intent.as_str() {
                "feature" => Intent::Feature,
                "bugfix" => Intent::Bugfix,
                "refactor" => Intent::Refactor,
                "spike" => Intent::Spike,
                "review" => Intent::Review,
                _ => Intent::Other,
            };
            let complexity = match case.complexity.as_str() {
                "trivial" => Complexity::Trivial,
                "bounded" => Complexity::Bounded,
                "substantial" => Complexity::Substantial,
                _ => Complexity::Architectural,
            };
            let risk = match case.risk.as_str() {
                "low" => RiskBand::Low,
                "medium" => RiskBand::Medium,
                "high" => RiskBand::High,
                _ => RiskBand::Critical,
            };
            let classification = classification_with(intent, complexity, risk, case.changed_files);
            let profile = ExecutionProfile::derive(&case.objective, &classification);
            let plan = compile(
                &case.objective,
                &profile,
                &registry(),
                &skills(),
                &always_eligible,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.objective));
            assert!(
                plan.seats.len() <= case.max_team_size,
                "{}: {} seats over max {}: {plan:?}",
                case.objective,
                plan.seats.len(),
                case.max_team_size
            );
            let present: std::collections::BTreeSet<&str> = plan
                .seats
                .iter()
                .map(|seat| seat.manifest_id.as_str())
                .collect();
            for required in &case.required_manifest_ids {
                assert!(
                    present.contains(required.as_str()),
                    "{}: expected '{required}' among {present:?}",
                    case.objective
                );
            }
            for forbidden in &case.forbidden_manifest_ids {
                assert!(
                    !present.contains(forbidden.as_str()),
                    "{}: '{forbidden}' must not be present, got {present:?}",
                    case.objective
                );
            }
        }
    }
}
