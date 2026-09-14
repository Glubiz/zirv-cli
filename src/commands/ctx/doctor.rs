//! `zirv ctx doctor` (issue #491, roadmap N22): one command that answers
//! "why can't this machine run a native session?" in a vocabulary an operator
//! can act on.
//!
//! It adds no new discovery of its own. Every fact here already had an owner
//! before N22 -- `provider::inventory::Inventory` (routes, accounts, models,
//! billing, probes), `runtime::capabilities::discover` (MCP/web/browser/
//! diagnostics/artifact/frontend integrations), `runtime::enforcement::
//! PlatformIsolation::detect` (process containment), `adapters::ADAPTERS`
//! (which coding harnesses are actually installed), `config::RuntimeConfig`
//! (which backend an unflagged session gets). What this module contributes is
//! the *classification*: turning those reports into exactly one of six named
//! failure classes, so "no API key", "the model is not yours", "bwrap is not
//! installed", "the endpoint is down" and "this is an upstream entitlement,
//! not a zirv gap" can never be confused for one another.
//!
//! Mirrors the `rot.rs`/`score.rs` split CLAUDE.md requires: [`classify`] and
//! [`diagnose`] are pure -- given already-gathered reports they touch no fs,
//! clock, env or net -- while [`run`]/[`run_with`] gather and render.
//!
//! Everything rendered goes through [`super::snapshot::redact_text`] first,
//! the same `screen::screen` pass `zirv ctx snapshot` uses: a doctor report
//! is the thing operators paste into bug reports, so a credential-shaped
//! string, a transcript excerpt or an opaque continuation token must never
//! survive it (issue #491, implementation item 6).

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;
use serde::Serialize;

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::provider::config::NativeConfig;
use super::provider::credential::{CredentialStore, OsStore};
use super::provider::inventory::Inventory;
use super::provider::probe::{HttpProbe, Probe};
use super::runtime::enforcement::PlatformIsolation;
use super::{CtxResult, runtime, snapshot};
use crate::commands::workflow::capability::{IntegrationState, IntegrationStatus};

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Only report on this role's route, instead of every role.
    #[arg(long)]
    pub role: Option<String>,
    /// Contact each configured provider endpoint's model-list, so a route
    /// that looks credentialed can be told apart from one that actually
    /// answers. Makes network requests; off by default for that reason, the
    /// same opt-in `zirv ctx provider check --live` already is.
    #[arg(long)]
    pub live: bool,
    #[arg(long)]
    pub json: bool,
}

/// The six classes a native-readiness failure can be, and the whole point of
/// this module: an operator's next action is different for every one of them,
/// and conflating any two of them wastes their afternoon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingKind {
    /// No API key/token resolved, or the one that resolved was rejected.
    MissingAuthMaterial,
    /// Auth material exists; this model is not one this account may call --
    /// unknown id, ambiguous alias, or absent from the account's model list.
    InaccessibleModel,
    /// A binary, MCP server or configured integration zirv needs is not
    /// installed or not configured. A missing native ADAPTER lands here too:
    /// it is a zirv gap, never an entitlement excuse (issue #491, item 7).
    MissingTool,
    /// No verified process-containment mechanism on this platform.
    UnsupportedIsolation,
    /// The endpoint is configured and credentialed but did not serve the
    /// request: unreachable, TLS refused, or a non-auth HTTP status.
    ServiceFailure,
    /// A genuine upstream limitation, not an implementation gap: a
    /// subscription-billed account with no API entitlement, or a vendor
    /// surface that exists only inside that vendor's own CLI.
    UpstreamEntitlement,
}

impl FindingKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MissingAuthMaterial => "missing-auth-material",
            Self::InaccessibleModel => "inaccessible-model",
            Self::MissingTool => "missing-tool",
            Self::UnsupportedIsolation => "unsupported-isolation",
            Self::ServiceFailure => "service-failure",
            Self::UpstreamEntitlement => "upstream-entitlement",
        }
    }
}

/// Whether a finding stops native work or merely narrows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A role has no usable route: a native session for it cannot start.
    Blocking,
    /// Native sessions still run; something optional is unavailable and the
    /// surface that needs it refuses at use time instead.
    Advisory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub severity: Severity,
    /// What the finding is about: a route id, a role, an integration name.
    pub subject: String,
    /// The reporting module's own message, verbatim except for redaction.
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoleRow {
    pub role: String,
    pub runtime: String,
    pub runtime_source: String,
    pub route: Option<String>,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    /// Whether `~/.zirv/native.toml` exists and parsed at all.
    pub native_configured: bool,
    /// Which coding harnesses are installed. Empty is a supported state, not
    /// an error: it is the state the whole native runtime exists to serve.
    pub harnesses_present: Vec<String>,
    pub isolation: String,
    pub roles: Vec<RoleRow>,
    pub findings: Vec<Finding>,
}

impl DoctorReport {
    pub fn blocking(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == Severity::Blocking)
    }
}

/// Pure: which class a route/integration problem message belongs to.
///
/// The messages it reads are produced a few hundred lines away in
/// `provider::inventory` and `runtime::capabilities`, so the markers matched
/// here are deliberately the distinctive noun phrases those modules commit
/// to, not incidental wording. `a_doctor_names_each_failure_class_from_a_real
/// _inventory` drives real configs through `Inventory::build` rather than
/// asserting on hand-written strings, so a reworded message that lands in the
/// wrong class fails the build instead of silently mislabelling an operator's
/// afternoon.
pub fn classify(message: &str) -> FindingKind {
    let lower = message.to_ascii_lowercase();
    // Order matters: several messages mention more than one noun, and the
    // most specific reading wins.
    if lower.contains("subscription-billed") || lower.contains("legacy-only upstream") {
        return FindingKind::UpstreamEntitlement;
    }
    if lower.contains("plaintext http") {
        return FindingKind::ServiceFailure;
    }
    if lower.contains("no adapter yet") || lower.contains("no route profile binds") {
        return FindingKind::MissingTool;
    }
    if lower.contains("credential") {
        return FindingKind::MissingAuthMaterial;
    }
    if lower.contains("model") {
        return FindingKind::InaccessibleModel;
    }
    if lower.contains("http") || lower.contains("unreachable") || lower.contains("probe") {
        return FindingKind::ServiceFailure;
    }
    FindingKind::MissingTool
}

/// Everything [`diagnose`] needs, already gathered. Separated from the
/// gathering so the classification can be tested without a network, a home
/// directory or an installed harness.
pub struct DoctorInput<'a> {
    pub native_configured: bool,
    pub inventory: Option<&'a Inventory>,
    pub integrations: &'a [IntegrationStatus],
    pub isolation: &'a PlatformIsolation,
    pub harnesses_present: Vec<String>,
    pub runtime: &'a super::config::RuntimeConfig,
    pub role_filter: Option<&'a str>,
}

/// Pure: no fs, clock, env or net.
pub fn diagnose(input: &DoctorInput<'_>) -> DoctorReport {
    let mut findings = Vec::new();
    let mut roles = Vec::new();
    let resolve_role = |role: &str| {
        runtime::resolve(runtime::CONFIGURED, input.runtime, role).unwrap_or(
            runtime::RuntimeChoice {
                kind: runtime::RuntimeKind::Harness,
                source: runtime::RuntimeSource::BuiltIn,
                note: None,
            },
        )
    };

    if let Some(inventory) = input.inventory {
        for row in &inventory.access {
            if input.role_filter.is_some_and(|role| row.role != role) {
                continue;
            }
            let choice = resolve_role(&row.role);
            roles.push(RoleRow {
                role: row.role.clone(),
                runtime: choice.kind.as_str().to_string(),
                runtime_source: choice.source.as_str().to_string(),
                route: row.route.as_ref().map(ToString::to_string),
                state: row.state_text.clone(),
            });
            // A role with no route at all is only blocking for an operator
            // who has asked for native somewhere: on a harness-default
            // machine it is simply "native is not set up for this role yet".
            if row.route.is_none() {
                let native_wanted = choice.kind == runtime::RuntimeKind::Native;
                findings.push(Finding {
                    kind: FindingKind::MissingTool,
                    severity: if native_wanted {
                        Severity::Blocking
                    } else {
                        Severity::Advisory
                    },
                    subject: format!("role {}", row.role),
                    detail: "no native route configured; add a `[route]` and name it under \
                             `[roles]` in ~/.zirv/native.toml"
                        .to_string(),
                });
            }
        }
        for route in &inventory.routes {
            let bound = inventory
                .access
                .iter()
                .any(|row| row.route.as_ref() == Some(&route.route));
            if input.role_filter.is_some()
                && !roles
                    .iter()
                    .any(|row| row.route.as_deref() == Some(route.route.as_ref()))
            {
                continue;
            }
            for problem in &route.problems {
                findings.push(Finding {
                    kind: classify(problem),
                    // A problem on a route no role names costs nothing until
                    // a role names it.
                    severity: if bound {
                        Severity::Blocking
                    } else {
                        Severity::Advisory
                    },
                    subject: format!("route {}", route.route),
                    detail: problem.clone(),
                });
            }
            if route.problems.is_empty()
                && route
                    .notes
                    .iter()
                    .any(|note| note.contains("model not listed"))
            {
                findings.push(Finding {
                    kind: FindingKind::InaccessibleModel,
                    severity: Severity::Advisory,
                    subject: format!("route {}", route.route),
                    detail: format!(
                        "model `{}` is not listed for this account (entitlement restriction or \
                         alias mismatch)",
                        route.model.id
                    ),
                });
            }
        }
    } else {
        // No native configuration at all is still worth a role table: "which
        // backend would an unflagged session get" is answerable before any
        // provider is declared, and it is the first thing an operator
        // migrating to native wants to see.
        for role in super::provider::inventory::DEFAULT_ROLES
            .iter()
            .copied()
            .chain(input.runtime.roles.keys().map(String::as_str))
            .collect::<std::collections::BTreeSet<_>>()
        {
            if input.role_filter.is_some_and(|filter| role != filter) {
                continue;
            }
            let choice = resolve_role(role);
            let native_wanted = choice.kind == runtime::RuntimeKind::Native;
            roles.push(RoleRow {
                role: role.to_string(),
                runtime: choice.kind.as_str().to_string(),
                runtime_source: choice.source.as_str().to_string(),
                route: None,
                state: "unconfigured".to_string(),
            });
            if native_wanted {
                findings.push(Finding {
                    kind: FindingKind::MissingTool,
                    severity: Severity::Blocking,
                    subject: format!("role {role}"),
                    detail: "configured to run natively, but there is no native provider \
                             configuration; run `zirv ctx provider init`"
                        .to_string(),
                });
            }
        }
        findings.push(Finding {
            kind: FindingKind::MissingTool,
            severity: Severity::Advisory,
            subject: "native.toml".to_string(),
            detail: "no native provider configuration; run `zirv ctx provider init`".to_string(),
        });
    }

    for integration in input.integrations {
        if integration.state != IntegrationState::Unavailable {
            continue;
        }
        let detail = integration
            .diagnosis
            .clone()
            .unwrap_or_else(|| integration.detail.clone());
        findings.push(Finding {
            kind: classify(&detail),
            severity: Severity::Advisory,
            subject: integration.integration.to_string(),
            detail,
        });
    }

    if let PlatformIsolation::Unavailable { platform, reason } = input.isolation {
        findings.push(Finding {
            kind: FindingKind::UnsupportedIsolation,
            // Advisory, not blocking: `enforcement` refuses a sandboxed
            // invocation at the point of use with a typed error, so a native
            // session still runs -- it just cannot contain a subprocess.
            severity: Severity::Advisory,
            subject: format!("isolation ({platform})"),
            detail: reason.clone(),
        });
    }

    DoctorReport {
        native_configured: input.native_configured,
        harnesses_present: input.harnesses_present.clone(),
        isolation: input.isolation.mechanism().to_string(),
        roles,
        findings,
    }
}

/// How a line of conversation opens. `screen::screen`'s own
/// `RoleMarkerMidText` deliberately fires only PAST the start of the text it
/// is given -- it exists to catch a forged turn boundary spliced into
/// something else -- and `redact_text` screens line by line, so a pasted
/// transcript, whose every turn starts its own line, slips through it
/// untouched. This is the complementary rule, and it is structural rather
/// than heuristic: a doctor finding is a diagnosis, so a line that opens like
/// a conversation turn is not one, whatever it says.
const CONVERSATION_OPENERS: &[&str] = &[
    "human:",
    "assistant:",
    "user:",
    "system:",
    "developer:",
    "tool:",
    "<|",
    "<system>",
    "[inst]",
];

fn looks_like_conversation(line: &str) -> bool {
    let lower = line.trim_start().to_ascii_lowercase();
    CONVERSATION_OPENERS
        .iter()
        .any(|opener| lower.starts_with(opener))
}

/// Every string that leaves this module does so through here. See the module
/// doc: a doctor report is pasted into bug reports.
fn redacted(text: &str) -> String {
    text.lines()
        .map(|line| {
            if looks_like_conversation(line) {
                "[redacted -- conversation excerpt]".to_string()
            } else {
                snapshot::redact_text(line)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn render_text(report: &DoctorReport, w: &mut dyn Write) -> CtxResult<()> {
    writeln!(
        w,
        "native provider config: {}",
        if report.native_configured {
            "~/.zirv/native.toml"
        } else {
            "absent (run `zirv ctx provider init`)"
        }
    )?;
    writeln!(
        w,
        "coding harnesses on PATH: {}",
        if report.harnesses_present.is_empty() {
            "none (native operation needs none)".to_string()
        } else {
            report.harnesses_present.join(", ")
        }
    )?;
    writeln!(w, "process isolation: {}", redacted(&report.isolation))?;
    writeln!(w, "\nROLE\tRUNTIME\tVIA\tROUTE\tSTATE")?;
    for row in &report.roles {
        writeln!(
            w,
            "{}\t{}\t{}\t{}\t{}",
            redacted(&row.role),
            row.runtime,
            row.runtime_source,
            redacted(row.route.as_deref().unwrap_or("-")),
            redacted(&row.state)
        )?;
    }
    writeln!(w, "\nCLASS\tSEVERITY\tSUBJECT\tDETAIL")?;
    for finding in &report.findings {
        writeln!(
            w,
            "{}\t{}\t{}\t{}",
            finding.kind.as_str(),
            match finding.severity {
                Severity::Blocking => "blocking",
                Severity::Advisory => "advisory",
            },
            redacted(&finding.subject),
            redacted(&finding.detail)
        )?;
    }
    writeln!(
        w,
        "\nverdict: {}",
        if report.blocking() {
            "native work is blocked"
        } else if report.findings.is_empty() {
            "ready"
        } else {
            "ready, with advisories"
        }
    )?;
    Ok(())
}

/// The JSON shape, redacted field by field. Not `serde_json::to_value` on the
/// report itself: that would emit the raw strings.
pub fn render_json(report: &DoctorReport, w: &mut dyn Write) -> CtxResult<()> {
    let value = serde_json::json!({
        "native_configured": report.native_configured,
        "harnesses_present": report.harnesses_present,
        "isolation": redacted(&report.isolation),
        "blocking": report.blocking(),
        "roles": report.roles.iter().map(|row| serde_json::json!({
            "role": redacted(&row.role),
            "runtime": row.runtime,
            "runtime_source": row.runtime_source,
            "route": row.route.as_deref().map(redacted),
            "state": redacted(&row.state),
        })).collect::<Vec<_>>(),
        "findings": report.findings.iter().map(|finding| serde_json::json!({
            "kind": finding.kind.as_str(),
            "severity": finding.severity,
            "subject": redacted(&finding.subject),
            "detail": redacted(&finding.detail),
        })).collect::<Vec<_>>(),
    });
    writeln!(w, "{}", serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

pub fn run(args: &DoctorArgs, w: &mut dyn Write) -> CtxResult<i32> {
    let repo = match &args.repo {
        Some(repo) => repo.clone(),
        None => std::env::current_dir()?,
    };
    let home = crate::utils::home_dir()?;
    let env = env_from_process();
    let store = OsStore::default();
    let probe = HttpProbe::default();
    run_with(
        args,
        w,
        &home,
        &repo,
        &env,
        &store,
        args.live.then_some(&probe as &dyn Probe),
        super::state::now_secs(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    args: &DoctorArgs,
    w: &mut dyn Write,
    home: &Path,
    repo: &Path,
    env: EnvLookup<'_>,
    store: &dyn CredentialStore,
    probe: Option<&dyn Probe>,
    now: u64,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load(repo, env)?;
    let native = NativeConfig::load(home, repo)?;
    let inventory = native
        .as_ref()
        .map(|native| Inventory::build(native, env, store, now, probe));
    let integrations = super::runtime::capabilities::discover(&cfg, repo);
    let isolation = PlatformIsolation::detect();
    let harnesses_present = super::adapters::all(cfg.agent_bin.as_deref())
        .into_iter()
        .filter(|adapter| adapter.ready().is_ok())
        .map(|adapter| adapter.name().to_string())
        .collect();
    let report = diagnose(&DoctorInput {
        native_configured: native.is_some(),
        inventory: inventory.as_ref(),
        integrations: &integrations,
        isolation: &isolation,
        harnesses_present,
        runtime: &cfg.runtime,
        role_filter: args.role.as_deref(),
    });
    if args.json {
        render_json(&report, w)?;
    } else {
        render_text(&report, w)?;
    }
    Ok(i32::from(report.blocking()))
}

#[cfg(test)]
mod tests {
    use super::super::provider::credential::FakeStore;
    use super::super::provider::probe::{FakeProbe, ProbeResult};
    use super::super::testenv::{HomeGuard, repo as test_repo};
    use super::*;

    fn write_native(home: &Path, text: &str) {
        let path = NativeConfig::operator_path(home);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn report_for(
        native_toml: &str,
        env: EnvLookup<'_>,
        probe: Option<&dyn Probe>,
    ) -> DoctorReport {
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let repo = test_repo();
        write_native(home.path(), native_toml);
        let native = NativeConfig::load(home.path(), repo.path())
            .expect("native config")
            .expect("some");
        let inventory = Inventory::build(&native, env, &FakeStore::default(), 0, probe);
        diagnose(&DoctorInput {
            native_configured: true,
            inventory: Some(&inventory),
            integrations: &[],
            isolation: &PlatformIsolation::Unavailable {
                platform: "test".into(),
                reason: "no verified containment here".into(),
            },
            harnesses_present: Vec::new(),
            runtime: &super::super::config::RuntimeConfig::default(),
            role_filter: None,
        })
    }

    fn kinds_for(report: &DoctorReport) -> Vec<FindingKind> {
        report.findings.iter().map(|f| f.kind).collect()
    }

    /// The acceptance criterion this whole module exists for: the five
    /// failure classes an operator's next action differs on are told apart,
    /// and they are told apart from REAL inventory output rather than from
    /// strings this test wrote itself.
    #[test]
    fn a_doctor_names_each_failure_class_from_a_real_inventory() {
        // 1. Missing auth material: the env var the account names is unset.
        let missing = report_for(
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:ABSENT_KEY'\n\
             [route.work]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='work'\n",
            &|_| None,
            None,
        );
        assert!(
            kinds_for(&missing).contains(&FindingKind::MissingAuthMaterial),
            "{:?}",
            missing.findings
        );

        // 2. Inaccessible model: auth material resolves, the endpoint answers
        //    200, and the account's own model list does not contain it. (An
        //    id no catalogue knows is refused by `NativeConfig::load` itself,
        //    before a doctor ever runs -- that is a config error, not a
        //    readiness finding.)
        let other_models = FakeProbe::new(ProbeResult::Http {
            status: 200,
            model_ids: vec!["claude-opus-5".into()],
        });
        let inaccessible = report_for(
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n\
             [route.work]\naccount='work'\nmodel='claude-sonnet-5'\n[roles]\nworker='work'\n",
            &|name| (name == "KEY").then(|| "sk-ant-doctor-test".to_string()),
            Some(&other_models),
        );
        assert!(
            kinds_for(&inaccessible).contains(&FindingKind::InaccessibleModel),
            "{:?}",
            inaccessible.findings
        );
        // And it is NOT mistaken for a missing key: the key worked.
        assert!(
            !kinds_for(&inaccessible).contains(&FindingKind::MissingAuthMaterial),
            "{:?}",
            inaccessible.findings
        );

        // 3. Upstream entitlement, NOT missing auth material: a
        //    subscription-billed account has no API entitlement to give.
        let subscription = report_for(
            "schema=1\n[account.sub]\nprovider='anthropic'\nbilling='subscription'\n\
             [route.sub]\naccount='sub'\nmodel='sonnet'\n[roles]\nworker='sub'\n",
            &|_| None,
            None,
        );
        assert!(
            kinds_for(&subscription).contains(&FindingKind::UpstreamEntitlement),
            "{:?}",
            subscription.findings
        );

        // 4. Service failure: credentialed, but the endpoint does not answer.
        let offline = FakeProbe::new(ProbeResult::Unreachable("connection refused".into()));
        let unreachable = report_for(
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n\
             [route.work]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='work'\n",
            &|name| (name == "KEY").then(|| "sk-ant-doctor-test".to_string()),
            Some(&offline),
        );
        assert!(
            kinds_for(&unreachable).contains(&FindingKind::ServiceFailure),
            "{:?}",
            unreachable.findings
        );

        // 5. Unsupported isolation, in every one of them: the fixture's
        //    platform reports no verified containment.
        assert!(
            kinds_for(&missing).contains(&FindingKind::UnsupportedIsolation),
            "{:?}",
            missing.findings
        );
    }

    /// Auth material that is present but REJECTED is still an auth-material
    /// finding, not a service failure: the endpoint answered fine.
    #[test]
    fn a_rejected_secret_is_auth_material_not_a_service_failure() {
        let rejected = FakeProbe::new(ProbeResult::Http {
            status: 401,
            model_ids: Vec::new(),
        });
        let report = report_for(
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n\
             [route.work]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='work'\n",
            &|name| (name == "KEY").then(|| "sk-ant-doctor-test".to_string()),
            Some(&rejected),
        );
        let route_findings: Vec<_> = report
            .findings
            .iter()
            .filter(|finding| finding.subject.starts_with("route "))
            .collect();
        assert_eq!(
            route_findings
                .iter()
                .map(|finding| finding.kind)
                .collect::<Vec<_>>(),
            vec![FindingKind::MissingAuthMaterial],
            "{route_findings:?}"
        );
    }

    /// Before any provider is declared, the report still answers the first
    /// question a migrating operator has -- and an operator who switched the
    /// default to native with nothing configured is BLOCKED, not merely
    /// advised.
    #[test]
    fn an_unconfigured_machine_still_reports_the_backend_each_role_would_get() {
        let runtime: super::super::config::RuntimeConfig =
            toml::from_str("default = 'native'\n").expect("runtime table");
        let report = diagnose(&DoctorInput {
            native_configured: false,
            inventory: None,
            integrations: &[],
            isolation: &PlatformIsolation::Unavailable {
                platform: "test".into(),
                reason: "none".into(),
            },
            harnesses_present: Vec::new(),
            runtime: &runtime,
            role_filter: None,
        });
        let worker = report
            .roles
            .iter()
            .find(|row| row.role == "worker")
            .expect("worker row");
        assert_eq!(worker.runtime, "native");
        assert_eq!(worker.runtime_source, "runtime.default");
        assert!(report.blocking(), "{:?}", report.findings);
    }

    /// A missing integration is a missing TOOL, and an unavailable one never
    /// blocks: the surface that needs it refuses at use time.
    #[test]
    fn an_unavailable_integration_is_an_advisory_missing_tool() {
        let integrations = vec![IntegrationStatus::unavailable(
            crate::commands::workflow::capability::IntegrationId::Browser,
            "no browser binary",
            "install chromium or set capabilities.browser.binary",
        )];
        let report = diagnose(&DoctorInput {
            native_configured: true,
            inventory: None,
            integrations: &integrations,
            isolation: &PlatformIsolation::Unavailable {
                platform: "test".into(),
                reason: "none".into(),
            },
            harnesses_present: Vec::new(),
            runtime: &super::super::config::RuntimeConfig::default(),
            role_filter: None,
        });
        let browser = report
            .findings
            .iter()
            .find(|finding| finding.subject == "browser")
            .expect("browser finding");
        assert_eq!(browser.kind, FindingKind::MissingTool);
        assert_eq!(browser.severity, Severity::Advisory);
        assert!(!report.blocking());
    }

    /// Issue #491, implementation item 6: a diagnostics dump must identify
    /// route/auth/capability failures WITHOUT carrying a secret, a private
    /// transcript excerpt or opaque continuation data out with it.
    #[test]
    fn a_diagnostic_dump_carries_no_secret_transcript_or_continuation_data() {
        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let transcript = "Human: the deploy key is under /etc\n\nAssistant: understood, I will";
        let continuation =
            "resume_token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.QUJDREVGR0hJSktMTU5PUFFSU1RVVldY";
        let report = DoctorReport {
            native_configured: true,
            harnesses_present: Vec::new(),
            isolation: "unavailable".into(),
            roles: Vec::new(),
            findings: vec![
                Finding {
                    kind: FindingKind::MissingAuthMaterial,
                    severity: Severity::Blocking,
                    subject: "route work".into(),
                    detail: format!("credential rejected: {secret}"),
                },
                Finding {
                    kind: FindingKind::ServiceFailure,
                    severity: Severity::Advisory,
                    subject: "route work".into(),
                    detail: transcript.into(),
                },
                Finding {
                    kind: FindingKind::ServiceFailure,
                    severity: Severity::Advisory,
                    subject: "route work".into(),
                    detail: continuation.into(),
                },
            ],
        };
        for render in [
            render_text as fn(&DoctorReport, &mut dyn Write) -> CtxResult<()>,
            render_json,
        ] {
            let mut out = Vec::new();
            render(&report, &mut out).expect("render");
            let text = String::from_utf8(out).expect("utf8");
            assert!(!text.contains(secret), "secret leaked: {text}");
            assert!(
                !text.contains("the deploy key is under"),
                "transcript leaked: {text}"
            );
            assert!(
                !text.contains("QUJDREVGR0hJSktMTU5PUFFSU1RVVldY"),
                "continuation data leaked: {text}"
            );
            // The classification itself still survives redaction -- a dump
            // that says nothing is not a diagnostic.
            assert!(text.contains("missing-auth-material"), "{text}");
        }
    }

    /// The end-to-end CLI path, with no coding harness assumed and no
    /// network: exit 0 when nothing blocks, and the role table shows which
    /// authority chose each backend.
    #[test]
    fn the_command_reports_the_resolved_backend_and_exits_zero_without_blockers() {
        let home = tempfile::tempdir().expect("home");
        let _home = HomeGuard::set(home.path());
        let repo = test_repo();
        write_native(
            home.path(),
            "schema=1\n[account.work]\nprovider='anthropic'\ncredential='env:KEY'\n\
             [route.work]\naccount='work'\nmodel='sonnet'\n[roles]\nworker='work'\n",
        );
        std::fs::write(
            home.path().join(".zirv").join("ctx.toml"),
            "[runtime]\ndefault = 'native'\n",
        )
        .expect("ctx.toml");
        let mut out = Vec::new();
        let code = run_with(
            &DoctorArgs {
                repo: Some(repo.path().to_path_buf()),
                role: Some("worker".into()),
                live: false,
                json: false,
            },
            &mut out,
            home.path(),
            repo.path(),
            &|name| (name == "KEY").then(|| "sk-ant-doctor-test".to_string()),
            &FakeStore::default(),
            None,
            0,
        )
        .expect("doctor");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("worker\tnative\truntime.default"), "{text}");
        assert_eq!(code, 0, "{text}");
    }
}
