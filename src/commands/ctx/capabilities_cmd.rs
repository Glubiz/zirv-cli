//! `zirv ctx capabilities` (issue #483, roadmap N14): the operator surface
//! for the capability report a native session and the workflow engine both
//! read.
//!
//! Every row is one of exactly three states. `available` means zirv found the
//! backend; `unavailable` means it did not, and the row names the missing
//! binary, credential or config key; `unverified` means it is configured but
//! has not been contacted this run. `--probe` is what converts an unverified
//! MCP row into a verified one, by actually connecting, negotiating and
//! discovering -- which is also the only thing in this command that starts a
//! process or reaches the network, and is therefore opt-in.

use std::io::Write;
use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use super::config::CtxConfig;
use super::runtime::capabilities::{self, CapabilityServices};
use super::{CtxResult, state};
use crate::commands::workflow::capability::{IntegrationState, IntegrationStatus};

#[derive(Debug, Args)]
pub struct CapabilitiesArgs {
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Contact every configured MCP server, so `unverified` rows become a
    /// verified `available` or `unavailable`. Starts processes and makes
    /// network requests; off by default for exactly that reason.
    #[arg(long)]
    pub probe: bool,
    /// Exit non-zero unless this integration would admit a workflow step --
    /// the same rule the engine applies, reachable from a script. Repeatable.
    #[arg(long = "require", value_name = "INTEGRATION")]
    pub require: Vec<String>,
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &CapabilitiesArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let repo = match &args.repo {
        Some(repo) => repo.clone(),
        None => std::env::current_dir()?,
    };
    let cfg = CtxConfig::load(&repo, &|key| std::env::var(key).ok())?;
    let mut rows = capabilities::discover(&cfg, &repo);
    let mut probes = Vec::new();
    if args.probe {
        let mut services = CapabilityServices::from_config(
            &cfg,
            &repo,
            &|key| std::env::var(key).ok(),
            state::now_secs(),
        );
        probes = probe_servers(&mut services);
        services.shutdown();
        apply_probe(&mut rows, &probes);
    }

    let mut refusals = Vec::new();
    for name in &args.require {
        let Some(integration) =
            crate::commands::workflow::capability::IntegrationId::parse(name)
        else {
            return Err(format!(
                "unknown integration {name:?}; expected one of {}",
                crate::commands::workflow::capability::IntegrationId::ALL
                    .map(|id| id.as_str())
                    .join(", ")
            )
            .into());
        };
        let row = rows
            .iter()
            .find(|row| row.integration == integration)
            .filter(|row| row.state.admits_step());
        if row.is_none() {
            refusals.push(format!(
                "{integration} is unavailable: {}",
                rows.iter()
                    .find(|row| row.integration == integration)
                    .and_then(|row| row.diagnosis.clone())
                    .unwrap_or_else(|| "no discovery row".into())
            ));
        }
    }

    if args.json {
        writeln!(
            writer,
            "{}",
            serde_json::to_string_pretty(&json!({
                "repo": repo.display().to_string(),
                "probed": args.probe,
                "integrations": rows,
                "servers": probes,
                "refusals": refusals,
            }))?
        )?;
    } else {
        for row in &rows {
            writeln!(
                writer,
                "{:<18} {:<12} {}",
                row.integration.to_string(),
                row.state.to_string(),
                row.detail
            )?;
            if let Some(diagnosis) = &row.diagnosis {
                writeln!(writer, "{:<31} {diagnosis}", "")?;
            }
        }
        for server in &probes {
            writeln!(
                writer,
                "mcp:{:<14} {:<12} {}",
                server["server"].as_str().unwrap_or_default(),
                server["state"].as_str().unwrap_or_default(),
                server["detail"].as_str().unwrap_or_default()
            )?;
        }
    }
    if refusals.is_empty() {
        // Zero even when something is unavailable: without `--require` this
        // command reports, it does not gate. Workflow admission is where an
        // unavailable integration stops something from happening.
        Ok(0)
    } else {
        if !args.json {
            for refusal in &refusals {
                writeln!(writer, "refused: {refusal}")?;
            }
        }
        Ok(1)
    }
}

fn probe_servers(services: &mut CapabilityServices) -> Vec<serde_json::Value> {
    services
        .server_names()
        .into_iter()
        .map(|name| match services.client(&name) {
            Ok(client) => json!({
                "server": name,
                "state": "available",
                "detail": format!(
                    "{} tool(s) over MCP {}",
                    client.catalogue().len(),
                    client.info().protocol_version
                ),
                "info": client.info(),
            }),
            Err(error) => json!({
                "server": name,
                "state": "unavailable",
                "detail": error.to_string(),
            }),
        })
        .collect()
}

/// Folds the probe result back into the MCP row, so the summary line never
/// disagrees with the per-server detail below it.
fn apply_probe(rows: &mut [IntegrationStatus], probes: &[serde_json::Value]) {
    use crate::commands::workflow::capability::IntegrationId;

    let Some(row) = rows
        .iter_mut()
        .find(|row| row.integration == IntegrationId::Mcp)
    else {
        return;
    };
    if probes.is_empty() || row.state == IntegrationState::Unavailable {
        return;
    }
    let reachable = probes
        .iter()
        .filter(|probe| probe["state"] == "available")
        .count();
    if reachable == probes.len() {
        row.state = IntegrationState::Available;
        row.detail = format!("{reachable} server(s) answered");
        row.diagnosis = None;
    } else {
        row.state = IntegrationState::Unavailable;
        row.diagnosis = Some(format!(
            "{} of {} configured server(s) did not answer",
            probes.len() - reachable,
            probes.len()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::capability::IntegrationId;

    #[test]
    fn an_unconfigured_machine_reports_every_integration_without_failing() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        let code = run(
            &CapabilitiesArgs {
                repo: Some(repo.path().to_path_buf()),
                probe: false,
                require: Vec::new(),
                json: true,
            },
            &mut out,
        )
        .expect("run");
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(value["probed"], false);
        let states: Vec<&str> = value["integrations"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| row["state"].as_str().unwrap_or_default())
            .collect();
        assert!(!states.is_empty());
        assert!(
            states
                .iter()
                .all(|state| ["available", "unavailable", "unverified"].contains(state)),
            "{states:?}"
        );
    }

    #[test]
    fn require_gates_on_the_same_rule_workflow_admission_uses() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        let code = run(
            &CapabilitiesArgs {
                repo: Some(repo.path().to_path_buf()),
                probe: false,
                require: vec!["web.search".into()],
                json: true,
            },
            &mut out,
        )
        .expect("run");
        assert_eq!(code, 1, "an unconfigured search must refuse");
        let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(value["refusals"].as_array().map(Vec::len), Some(1));

        let mut out = Vec::new();
        let code = run(
            &CapabilitiesArgs {
                repo: Some(repo.path().to_path_buf()),
                probe: false,
                require: vec!["artifact.render".into()],
                json: true,
            },
            &mut out,
        )
        .expect("run");
        assert_eq!(code, 0, "zirv's own artifact renderer is always available");
    }

    #[test]
    fn an_unknown_integration_name_is_an_error_rather_than_a_silent_pass() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        let error = run(
            &CapabilitiesArgs {
                repo: Some(repo.path().to_path_buf()),
                probe: false,
                require: vec!["browser.open".into()],
                json: true,
            },
            &mut out,
        )
        .expect_err("unknown name");
        assert!(error.to_string().contains("unknown integration"), "{error}");
    }

    #[test]
    fn a_probe_that_reaches_no_server_downgrades_the_summary_row_rather_than_claiming_success() {
        let mut rows = vec![IntegrationStatus::unverified(
            IntegrationId::Mcp,
            "1 configured server(s): docs",
            "not contacted",
        )];
        apply_probe(
            &mut rows,
            &[json!({"server": "docs", "state": "unavailable", "detail": "no such binary"})],
        );
        assert_eq!(rows[0].state, IntegrationState::Unavailable);
        assert!(
            rows[0]
                .diagnosis
                .as_deref()
                .is_some_and(|text| text.contains("did not answer")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_probe_that_reaches_every_server_verifies_the_summary_row() {
        let mut rows = vec![IntegrationStatus::unverified(
            IntegrationId::Mcp,
            "1 configured server(s): docs",
            "not contacted",
        )];
        apply_probe(
            &mut rows,
            &[json!({"server": "docs", "state": "available", "detail": "4 tool(s)"})],
        );
        assert_eq!(rows[0].state, IntegrationState::Available);
        assert!(rows[0].diagnosis.is_none());
    }
}
